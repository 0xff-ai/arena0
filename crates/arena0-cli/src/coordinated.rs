//! Coordination for the product-level multi-Host run path.
//!
//! This module is deliberately private to the CLI.  It composes the existing
//! daemon API; it does not introduce another execution, session, receipt, or
//! identity model.  One [`HostConnection`] represents one protocol
//! participant, and all asynchronous work is joined by the [`Coordinator`]
//! before the operation returns.

mod inspection;

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use arena0_client::answer;
use arena0_client::api::{
    ApiErrorCode, AwaitState, EnsembleSpec, EventData, EventFilter, HostRequest, NextEvent,
    ProgramDetail, ProgramSummary, ReceiptRef, Request, ResponseOk, VerifiedResult,
};
use arena0_client::proto::{DaemonClient, Subscription};
use arena0_client::protocol::{
    AbortKind, ColorDepth, ExecId, ExecLifecycle, PeerId, PendingId, ProgramHash, SessionHash,
    StopCause, View,
};
use arena0_home::HostName;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::agent::{DEFAULT_RESPONSE_TIMEOUT, ExecutableAgent};
use crate::progress::{RunProgress, RunStage, RunTerminalState};
use crate::tui::{
    PRIVATE_INSPECTION_LIMIT, RunUpdate, TuiCalloutRequest, TuiConfig, TuiDriver, TuiHandle,
    TuiHost, TuiSession,
};

const RECEIPT_RETRY_ATTEMPTS: usize = 20;
const RECEIPT_RETRY_DELAY: Duration = Duration::from_millis(150);
const CREATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CANCELLED_CREATE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
fn record_stage(
    operation: &'static str,
    started: Instant,
    hosts: usize,
    success: bool,
    program_id: Option<ProgramHash>,
    exec_id: Option<ExecId>,
    session_id: Option<SessionHash>,
) {
    if !tracing::enabled!(target: "arena0::performance", tracing::Level::DEBUG) {
        return;
    }
    let program_id = program_id.map(|id| id.to_string()).unwrap_or_default();
    let exec_id = exec_id.map(|id| id.to_string()).unwrap_or_default();
    let session_id = session_id.map(|id| id.to_string()).unwrap_or_default();
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(
        target: "arena0::performance",
        operation,
        hosts,
        success,
        elapsed_us,
        program_id,
        exec_id,
        session_id,
        "coordinated run stage"
    );
}

/// A CLI-local policy for answering a Host callout.
///
/// This is configuration, not a protocol identity or a participant role.  In
/// particular, the Host's canonical participant ordering is never inferred
/// from the order in which these values are supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverSpec {
    /// An independently connected MCP or monitor client supplies answers.
    External,
    /// Reserved for the focused TUI path.  The JSON coordinator rejects it
    /// until a caller supplies an interactive input function.
    Human,
    /// A deterministic policy selected by the CLI.
    Builtin(String),
    /// One directly executed JSONL agent process owned by this Host binding.
    Executable(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HumanFrontend {
    None,
    InlineSingle,
    SharedTui,
}

/// One validated CLI binding between a named Host and a local driver policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverBinding {
    pub(crate) host: HostName,
    pub(crate) driver: DriverSpec,
}

struct Callout<'a> {
    name: &'a str,
    prompt: &'a str,
    context: &'a Value,
    schema: &'a Value,
}

struct DriverAnswerContext<'a> {
    host: &'a HostName,
    client: &'a DaemonClient,
    exec_id: ExecId,
}

impl DriverBinding {
    /// Construct a binding from an already parsed Host name and driver.
    #[must_use]
    pub(crate) const fn new(host: HostName, driver: DriverSpec) -> Self {
        Self { host, driver }
    }
}

/// Driver bindings after local, side-effect-free validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedBindings(Vec<DriverBinding>);

impl ValidatedBindings {
    /// Validate the minimum participant and driver constraints.
    fn new(bindings: Vec<DriverBinding>, human_frontend: HumanFrontend) -> anyhow::Result<Self> {
        if bindings.len() < 2 {
            bail!("a coordinated run requires at least two distinct Hosts");
        }

        let mut names = HashSet::with_capacity(bindings.len());
        let mut human_count = 0usize;
        for binding in &bindings {
            if !names.insert(binding.host.clone()) {
                bail!("Host '{}' was selected more than once", binding.host);
            }
            match &binding.driver {
                DriverSpec::Human => {
                    human_count += 1;
                    if human_frontend == HumanFrontend::None {
                        bail!(
                            "human drivers require an interactive run; use built-in drivers with --json"
                        );
                    }
                }
                DriverSpec::Executable(path) if path.as_os_str().is_empty() => {
                    bail!(
                        "executable driver for Host '{}' has an empty path",
                        binding.host
                    );
                }
                DriverSpec::Builtin(strategy) => validate_builtin_strategy(strategy)?,
                DriverSpec::Executable(_) | DriverSpec::External => {}
            }
        }
        if human_frontend == HumanFrontend::InlineSingle && human_count > 1 {
            bail!("inline input supports at most one human driver; use the shared TUI");
        }

        Ok(Self(bindings))
    }

    #[must_use]
    pub(crate) fn as_slice(&self) -> &[DriverBinding] {
        &self.0
    }
}

/// Inputs to one coordinated local run.
#[derive(Debug, Clone)]
pub(crate) struct CoordinatedRunArgs {
    pub(crate) program: String,
    pub(crate) params: Option<Value>,
    pub(crate) bindings: Vec<DriverBinding>,
    pub(crate) replay: bool,
    pub(crate) use_tui: bool,
}

/// The terminal projection returned by one Host's `exec.next` stream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HostTerminal {
    Completed {
        session_id: SessionHash,
        outcome: Option<Value>,
    },
    Failed {
        reason: String,
    },
}

/// The one terminal form shared by every live Host projection.
#[derive(Debug, Clone, PartialEq)]
enum TerminalConsensus {
    Completed {
        session_id: SessionHash,
        outcome: Option<Value>,
    },
    Stopped,
}

/// The authenticated terminal form returned to the CLI after every Host
/// receipt agrees. Failures without receipt evidence never construct this type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AggregateTerminal {
    Completed { outcome: Option<Value> },
    Stopped,
    Failed,
}

impl AggregateTerminal {
    #[must_use]
    pub(crate) const fn tag(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "completed",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub(crate) const fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    const fn progress_state(&self) -> RunTerminalState {
        match self {
            Self::Completed { .. } => RunTerminalState::Succeeded,
            Self::Stopped | Self::Failed => RunTerminalState::Failed,
        }
    }
}

/// The exact artifact independently verified by one local Host.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HostEvidence {
    pub(crate) peer_id: PeerId,
    pub(crate) receipt_id: arena0_client::protocol::ReceiptId,
    pub(crate) program_id: ProgramHash,
    pub(crate) session_id: SessionHash,
    pub(crate) ensemble: Vec<PeerId>,
    pub(crate) steps: u64,
    pub(crate) result: VerifiedResult,
}

/// Shared facts recovered from every Host receipt.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EvidenceAgreement {
    pub(crate) receipt_id: arena0_client::protocol::ReceiptId,
    pub(crate) program_id: ProgramHash,
    pub(crate) session_id: SessionHash,
    pub(crate) ensemble: Vec<PeerId>,
    pub(crate) steps: u64,
    pub(crate) result: VerifiedResult,
}

/// One result document for the CLI to render.  It contains no receipt body and
/// no private driver data.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AggregateResult {
    pub(crate) receipt_id: arena0_client::protocol::ReceiptId,
    pub(crate) program_id: ProgramHash,
    pub(crate) session_id: SessionHash,
    pub(crate) participants: Vec<PeerId>,
    pub(crate) steps: u64,
    pub(crate) terminal: AggregateTerminal,
    pub(crate) verification: VerificationSummary,
    pub(crate) receipts: Vec<HostEvidence>,
}

/// Aggregate verification facts suitable for human or JSON presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationSummary {
    pub(crate) tier: VerificationTier,
    pub(crate) all_verified: bool,
    pub(crate) shared_evidence_agrees: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerificationTier {
    Light,
    Full,
}

impl VerificationTier {
    #[must_use]
    pub(crate) const fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }

    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Full => "full",
        }
    }
}

/// One Host's daemon identity and execution binding.
#[derive(Debug, Clone)]
struct HostConnection {
    host: HostName,
    client: DaemonClient,
    peer_id: PeerId,
    abi_version: u32,
    driver: DriverSpec,
}

/// One activated local participant.  `exec_id` exists only after the creator
/// or joiner request has succeeded; an activated participant always has a
/// durable shared `session_id` recovered from `exec.status`.
#[derive(Debug, Clone)]
struct LocalParticipant {
    host: HostName,
    client: DaemonClient,
    peer_id: PeerId,
    exec_id: ExecId,
    driver: DriverSpec,
}

/// Coordinates all Host requests and owns every spawned join task.
#[derive(Debug)]
struct Coordinator {
    participants: Vec<LocalParticipant>,
    progress: RunProgress,
}

impl Coordinator {
    async fn run(
        request: CoordinatedRunArgs,
        progress: RunProgress,
    ) -> anyhow::Result<AggregateResult> {
        let (signal, signal_received) = watch::channel(None::<String>);
        let signal_task = tokio::spawn(async move {
            let reason = match tokio::signal::ctrl_c().await {
                Ok(()) => "received Ctrl-C".to_owned(),
                Err(error) => format!("coordinated run cancellation failed: {error:#}"),
            };
            signal.send_replace(Some(reason));
        });
        let result = Self::run_with_signal(request, signal_received, progress.clone()).await;
        signal_task.abort();
        let _ = signal_task.await;
        let terminal = result.as_ref().map_or(RunTerminalState::Failed, |result| {
            result.terminal.progress_state()
        });
        progress.terminal(terminal);
        result
    }

    async fn run_with_signal(
        request: CoordinatedRunArgs,
        mut signal_received: watch::Receiver<Option<String>>,
        progress: RunProgress,
    ) -> anyhow::Result<AggregateResult> {
        let has_human = request
            .bindings
            .iter()
            .any(|binding| matches!(binding.driver, DriverSpec::Human));
        let human_frontend = if request.use_tui {
            HumanFrontend::SharedTui
        } else if has_human {
            HumanFrontend::InlineSingle
        } else {
            HumanFrontend::None
        };
        let bindings = ValidatedBindings::new(request.bindings, human_frontend)?;
        let started = Instant::now();
        let connected = progress
            .during(RunStage::Connecting, bindings.as_slice().len(), async {
                tokio::select! {
                    result = connect_hosts(&bindings, &progress) => result,
                    reason = wait_for_cancel_reason(&mut signal_received) => {
                        progress.terminal(RunTerminalState::Cancelled);
                        Err(anyhow!(reason))
                    },
                }
            })
            .await;
        record_stage(
            "connect_hosts",
            started,
            bindings.as_slice().len(),
            connected.is_ok(),
            None,
            None,
            None,
        );
        let connections = connected?;
        let started = Instant::now();
        let resolved = progress
            .during(RunStage::ProgramResolution, connections.len(), async {
                tokio::select! {
                    result = resolve_program(&connections, &request.program, &progress) => result,
                    reason = wait_for_cancel_reason(&mut signal_received) => {
                        progress.terminal(RunTerminalState::Cancelled);
                        Err(anyhow!(reason))
                    },
                }
            })
            .await;
        record_stage(
            "resolve_program",
            started,
            connections.len(),
            resolved.is_ok(),
            resolved
                .as_ref()
                .ok()
                .map(|program| program.summary.program_hash),
            None,
            None,
        );
        let program = resolved?;
        let program_id = program.summary.program_hash;
        validate_participant_count(&program.summary, connections.len())?;

        let tui_subscriptions = if request.use_tui {
            tokio::select! {
                result = open_tui_subscriptions(&connections) => result?,
                reason = wait_for_cancel_reason(&mut signal_received) => {
                    progress.terminal(RunTerminalState::Cancelled);
                    return Err(anyhow!(reason));
                }
            }
        } else {
            Vec::new()
        };

        let started = Instant::now();
        let created = progress
            .during(
                RunStage::Negotiation,
                connections.len(),
                create_executions(
                    &connections,
                    program_id,
                    request.params,
                    &mut signal_received,
                    &progress,
                ),
            )
            .await;
        record_stage(
            "negotiate",
            started,
            connections.len(),
            created.is_ok(),
            Some(program_id),
            created
                .as_ref()
                .ok()
                .and_then(|participants| participants.first().map(|entry| entry.exec_id)),
            None,
        );
        if created.is_err() && signal_received.borrow().is_some() {
            progress.terminal(RunTerminalState::Cancelled);
        }
        let participants = created?;
        let coordinator = Self {
            participants,
            progress,
        };
        let (cancel, cancelled) = watch::channel(false);
        let (user_cancel, mut user_cancelled) = watch::channel(None::<String>);
        let mut tui = if request.use_tui {
            if !coordinator
                .participants
                .iter()
                .any(|participant| matches!(participant.driver, DriverSpec::Human))
            {
                bail!("run TUI requires at least one human driver");
            }
            let hosts = coordinator
                .participants
                .iter()
                .map(|participant| TuiHost {
                    host: participant.host.clone(),
                    peer_id: participant.peer_id,
                    driver: match &participant.driver {
                        DriverSpec::Human => TuiDriver::Human,
                        DriverSpec::Builtin(strategy) => TuiDriver::Builtin(strategy.clone()),
                        DriverSpec::Executable(_) => TuiDriver::Agent,
                        DriverSpec::External => TuiDriver::External,
                    },
                })
                .collect();
            Some(TuiSession::start(
                TuiConfig {
                    program: program.summary.display_name.clone(),
                    hosts,
                    message_schema: program
                        .schema
                        .messages
                        .first()
                        .map(|message| message.borsh.clone()),
                },
                user_cancel,
            ))
        } else {
            None
        };
        let tui_handle = tui.as_ref().map(TuiSession::handle);
        enum RunOutcome {
            Finished(anyhow::Result<AggregateResult>),
            Cancelled(anyhow::Result<String>),
        }
        let mut operation = Box::pin(coordinator.complete(
            program_id,
            request.replay,
            cancel.clone(),
            cancelled,
            tui_handle,
            tui_subscriptions,
        ));
        let outcome = tokio::select! {
            biased;
            reason = wait_for_cancel_reason(&mut user_cancelled) => RunOutcome::Cancelled(Ok(reason)),
            result = &mut operation => RunOutcome::Finished(result),
            reason = wait_for_cancel_reason(&mut signal_received) => {
                RunOutcome::Cancelled(Ok(reason))
            },
        };
        match outcome {
            RunOutcome::Finished(Ok(result)) => {
                if let Some(tui) = &mut tui {
                    match &result.terminal {
                        AggregateTerminal::Completed { outcome } => {
                            tui.complete(outcome.clone()).await?
                        }
                        AggregateTerminal::Stopped => {
                            tui.stop("run stopped; Host receipts verified".to_owned())
                                .await?;
                        }
                        AggregateTerminal::Failed => {
                            tui.fail("run failed; Host receipts verified".to_owned())
                                .await?;
                        }
                    }
                }
                Ok(result)
            }
            RunOutcome::Finished(Err(error)) => {
                cancel.send_replace(true);
                let cleanup = coordinator.stop_all().await;
                let executions = coordinator.execution_refs();
                if let Some(tui) = &mut tui {
                    let summary = if cleanup.is_ok() {
                        format!("run failed: {error:#}; owned executions were stopped")
                    } else {
                        format!("run failed: {error:#}; execution cleanup was incomplete")
                    };
                    let _ = tui.fail(summary).await;
                }
                if let Err(cleanup) = cleanup {
                    return Err(error.context(format!(
                        "execution cleanup was incomplete: {cleanup:#}; durable executions: {executions}"
                    )));
                }
                Err(error.context(format!("durable executions: {executions}")))
            }
            RunOutcome::Cancelled(reason) => {
                let reason = reason.unwrap_or_else(|error| {
                    format!("coordinated run cancellation failed: {error:#}")
                });
                coordinator.progress.terminal(RunTerminalState::Cancelled);
                cancel.send_replace(true);
                let drained = tokio::time::timeout(Duration::from_secs(3), &mut operation).await;
                drop(operation);
                if let Some(tui) = &mut tui {
                    let _ = tui.close().await;
                }
                let cleanup = coordinator.stop_all().await;
                let executions = coordinator.execution_refs();
                match (drained, cleanup) {
                    (Err(_), _) => bail!(
                        "{reason}; local drivers did not stop within 3s; durable executions: {executions}"
                    ),
                    (_, Err(error)) => bail!(
                        "{reason}; cleanup was incomplete ({error:#}); durable executions: {executions}"
                    ),
                    (_, Ok(())) => {
                        bail!("{reason}; durable executions: {executions}")
                    }
                }
            }
        }
    }

    fn execution_refs(&self) -> String {
        self.participants
            .iter()
            .map(|participant| format!("{}={}", participant.host, participant.exec_id))
            .collect::<Vec<_>>()
            .join(", ")
    }

    async fn complete(
        &self,
        program_id: ProgramHash,
        replay: bool,
        cancel: watch::Sender<bool>,
        cancelled: watch::Receiver<bool>,
        tui: Option<TuiHandle>,
        tui_subscriptions: Vec<Subscription>,
    ) -> anyhow::Result<AggregateResult> {
        let Some(tui_handle) = tui.clone() else {
            return self
                .complete_inner(program_id, replay, cancel, cancelled, None)
                .await;
        };
        if tui_subscriptions.len() != self.participants.len() {
            bail!("run TUI lost a Host event subscription");
        }
        let sources = self
            .participants
            .iter()
            .zip(tui_subscriptions)
            .map(|(participant, subscription)| TuiEventSource {
                host: participant.host.clone(),
                client: participant.client.clone(),
                exec_id: participant.exec_id,
                subscription,
            })
            .collect();
        let (stop_observer, observer_stopped) = watch::channel(false);
        let operation = self.complete_inner(
            program_id,
            replay,
            cancel,
            cancelled.clone(),
            Some(tui_handle.clone()),
        );
        let observe = observe_tui(sources, tui_handle, observer_stopped);
        tokio::pin!(operation);
        tokio::pin!(observe);
        tokio::select! {
            result = &mut operation => {
                stop_observer.send_replace(true);
                match (result, observe.await) {
                    (Ok(result), Ok(())) => Ok(result),
                    (Err(error), Ok(())) => Err(error),
                    (Ok(_), Err(error)) => Err(error.context("stop run TUI observation")),
                    (Err(error), Err(observer)) => Err(error.context(format!(
                        "run TUI observation cleanup failed: {observer:#}"
                    ))),
                }
            }
            result = &mut observe => {
                result?;
                bail!("run TUI observation stopped before coordinated execution")
            }
        }
    }

    async fn complete_inner(
        &self,
        program_id: ProgramHash,
        replay: bool,
        cancel: watch::Sender<bool>,
        mut cancelled: watch::Receiver<bool>,
        tui: Option<TuiHandle>,
    ) -> anyhow::Result<AggregateResult> {
        let started = Instant::now();
        let activated = self
            .progress
            .during(RunStage::Activation, self.participants.len(), async {
                tokio::select! {
                    result = await_activation(&self.participants, &self.progress) => result,
                    () = wait_for_cancel(&mut cancelled) => {
                        Err(anyhow!("coordinated run cancelled during activation"))
                    },
                }
            })
            .await;
        record_stage(
            "activate",
            started,
            self.participants.len(),
            activated.is_ok(),
            Some(program_id),
            Some(self.participants[0].exec_id),
            activated.as_ref().ok().copied(),
        );
        let session_id = activated?;
        let started = Instant::now();
        let driven = self
            .progress
            .during(
                RunStage::Execution,
                0,
                self.drive(cancel, cancelled, tui.clone()),
            )
            .await;
        record_stage(
            "execute",
            started,
            self.participants.len(),
            driven.is_ok(),
            Some(program_id),
            Some(self.participants[0].exec_id),
            Some(session_id),
        );
        let terminals = driven?;
        let terminal = compare_terminals(&terminals)?;
        if let TerminalConsensus::Completed {
            session_id: terminal_session,
            ..
        } = &terminal
            && *terminal_session != session_id
        {
            bail!(
                "terminal session {} disagrees with activated session {}",
                terminal_session,
                session_id
            );
        }

        let tier = if replay {
            VerificationTier::Full
        } else {
            VerificationTier::Light
        };
        let started = Instant::now();
        let receipt_stage = if tier.is_full() {
            RunStage::Replay
        } else {
            RunStage::Verification
        };
        let verified = self
            .progress
            .during(
                receipt_stage,
                self.participants.len(),
                self.verify_receipts(session_id, tier, tui.clone()),
            )
            .await;
        record_stage(
            if tier.is_full() {
                "replay_receipts"
            } else {
                "verify_receipts"
            },
            started,
            self.participants.len(),
            verified.is_ok(),
            Some(program_id),
            Some(self.participants[0].exec_id),
            Some(session_id),
        );
        let receipts = verified?;
        let agreement = compare_evidence(&receipts)?;
        if agreement.program_id != program_id {
            bail!(
                "receipt program {} disagrees with selected program {}",
                agreement.program_id,
                program_id
            );
        }
        if agreement.session_id != session_id {
            bail!(
                "receipt session {} disagrees with activated session {}",
                agreement.session_id,
                session_id
            );
        }
        if agreement.ensemble.len() != self.participants.len() {
            bail!(
                "receipt ensemble has {} participants, expected {}",
                agreement.ensemble.len(),
                self.participants.len()
            );
        }
        let terminal = bind_verified_terminal(terminal, &agreement.result)?;

        Ok(AggregateResult {
            receipt_id: agreement.receipt_id,
            program_id,
            session_id,
            participants: agreement.ensemble,
            steps: agreement.steps,
            terminal,
            verification: VerificationSummary {
                tier,
                all_verified: true,
                shared_evidence_agrees: true,
            },
            receipts,
        })
    }

    async fn stop_all(&self) -> anyhow::Result<()> {
        stop_participants(&self.participants).await
    }

    async fn drive(
        &self,
        cancel: watch::Sender<bool>,
        cancelled: watch::Receiver<bool>,
        tui: Option<TuiHandle>,
    ) -> anyhow::Result<Vec<HostTerminal>> {
        let mut jobs = JoinSet::new();
        for (index, participant) in self.participants.iter().enumerate() {
            let client = participant.client.clone();
            let host = participant.host.clone();
            let exec_id = participant.exec_id;
            let driver = participant.driver.clone();
            let cancelled = cancelled.clone();
            let progress = self.progress.clone();
            let tui = matches!(driver, DriverSpec::Human)
                .then(|| tui.clone())
                .flatten();
            jobs.spawn(async move {
                drive_to_terminal(host, client, exec_id, driver, cancelled, tui, progress)
                    .await
                    .map(|terminal| (index, terminal))
            });
        }

        let mut terminals = vec![None; self.participants.len()];
        let mut first_error = None;
        while let Some(joined) = jobs.join_next().await {
            match joined {
                Ok(Ok((index, terminal))) => terminals[index] = Some(terminal),
                Ok(Err(error)) => {
                    cancel.send_replace(true);
                    first_error.get_or_insert(error);
                }
                Err(error) => {
                    cancel.send_replace(true);
                    first_error.get_or_insert_with(|| {
                        anyhow!("coordinated Host drive task failed to join: {error}")
                    });
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }

        terminals
            .into_iter()
            .map(|terminal| terminal.ok_or_else(|| anyhow!("coordinator lost a Host terminal")))
            .collect()
    }

    async fn verify_receipts(
        &self,
        session_id: SessionHash,
        tier: VerificationTier,
        tui: Option<TuiHandle>,
    ) -> anyhow::Result<Vec<HostEvidence>> {
        let mut jobs = JoinSet::new();
        for participant in &self.participants {
            let client = participant.client.clone();
            let peer_id = participant.peer_id;
            let host = participant.host.clone();
            jobs.spawn(async move {
                verify_one_receipt(host.clone(), client, session_id, peer_id, tier)
                    .await
                    .map(|evidence| (host, peer_id, evidence))
            });
        }

        let mut evidence = Vec::with_capacity(self.participants.len());
        let mut verified = 0;
        while let Some(joined) = jobs.join_next().await {
            self.progress.advance();
            let (host, peer_id, receipt) =
                joined.context("coordinated receipt verification task failed to join")??;
            verified += 1;
            if let Some(tui) = &tui {
                tui.update(RunUpdate::ReceiptVerified {
                    host,
                    peer_id,
                    tier: tier.as_str(),
                })
                .await?;
                tui.update(RunUpdate::VerificationProgress {
                    verified,
                    total: self.participants.len(),
                    tier: tier.as_str(),
                })
                .await?;
            }
            evidence.push(receipt);
        }
        evidence.sort_by_key(|entry| entry.peer_id);
        Ok(evidence)
    }
}

async fn stop_participants(participants: &[LocalParticipant]) -> anyhow::Result<()> {
    let mut jobs = JoinSet::new();
    for participant in participants {
        let client = participant.client.clone();
        let host = participant.host.clone();
        let exec_id = participant.exec_id;
        jobs.spawn(async move {
            let operation = async {
                match client
                    .call_host_raw(&host, &HostRequest::ExecCancelCreation { exec_id })
                    .await?
                {
                    Ok(ResponseOk::Ack) => Ok(()),
                    Ok(other) => bail!(
                        "unexpected creation-cancel response while stopping Host '{host}': {other:?}"
                    ),
                    Err(error) if error.code == ApiErrorCode::NotFound => {
                        match client
                            .call_host_raw(&host, &HostRequest::ExecStatus { exec_id })
                            .await?
                        {
                            Ok(ResponseOk::Status(status)) if status.lifecycle().is_terminal() => {
                                Ok(())
                            }
                            Ok(ResponseOk::Status(_)) | Err(_) => Err(anyhow!(error)),
                            Ok(other) => bail!(
                                "unexpected status response while stopping Host '{host}': {other:?}"
                            ),
                        }
                    }
                    Err(error) => Err(anyhow!(error)),
                }
            };
            tokio::time::timeout(Duration::from_secs(2), operation)
                .await
                .map_err(|_| anyhow!("timed out stopping Host '{host}' execution {exec_id}"))?
        });
    }

    let mut failures = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(format!("{error:#}")),
            Err(error) => failures.push(format!("stop task failed to join: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

/// Run one coordinated local execution through the existing Host API.
pub(crate) async fn run(
    request: CoordinatedRunArgs,
    progress: RunProgress,
) -> anyhow::Result<AggregateResult> {
    Coordinator::run(request, progress).await
}

async fn connect_hosts(
    bindings: &ValidatedBindings,
    progress: &RunProgress,
) -> anyhow::Result<Vec<HostConnection>> {
    let client = DaemonClient::from_env()
        .map_err(|error| anyhow!("resolve daemon socket from environment: {error}"))?;
    let daemon_abi_version = match client.call(&Request::DaemonInfo).await? {
        ResponseOk::DaemonInfo(info) => info.abi_version,
        other => bail!("unexpected daemon.info response: {other:?}"),
    };
    let mut jobs = JoinSet::new();
    for (index, binding) in bindings.as_slice().iter().enumerate() {
        let host = binding.host.clone();
        let driver = binding.driver.clone();
        let client = client.clone();
        jobs.spawn(async move {
            let info = match client.call_host(&host, &HostRequest::Info).await? {
                ResponseOk::HostStatus(status) => status.host,
                other => bail!("unexpected host.info response from Host '{host}': {other:?}"),
            };
            Ok::<_, anyhow::Error>((
                index,
                HostConnection {
                    host,
                    client,
                    peer_id: info.peer_id,
                    abi_version: daemon_abi_version,
                    driver,
                },
            ))
        });
    }

    let mut ordered = (0..bindings.as_slice().len())
        .map(|_| None)
        .collect::<Vec<Option<HostConnection>>>();
    while let Some(joined) = jobs.join_next().await {
        progress.advance();
        let (index, connection) = joined.context("Host connection task failed to join")??;
        ordered[index] = Some(connection);
    }

    let mut peers = HashSet::with_capacity(ordered.len());
    let mut connections = Vec::with_capacity(ordered.len());
    for (index, connection) in ordered.into_iter().enumerate() {
        let connection = connection.ok_or_else(|| anyhow!("Host connection {index} missing"))?;
        if !peers.insert(connection.peer_id) {
            bail!(
                "Hosts '{}' and another selected Host share PeerId {}; each participant needs an independent identity",
                connection.host,
                connection.peer_id
            );
        }
        connections.push(connection);
    }
    Ok(connections)
}

async fn open_tui_subscriptions(
    connections: &[HostConnection],
) -> anyhow::Result<Vec<Subscription>> {
    let filter = EventFilter::try_new(vec!["exec.*".to_owned(), "host.*".to_owned()], Vec::new())?;
    let mut jobs = JoinSet::new();
    for (index, connection) in connections.iter().enumerate() {
        let client = connection.client.clone();
        let host = connection.host.clone();
        let filter = filter.clone();
        jobs.spawn(async move {
            client
                .subscribe(&host, filter)
                .await
                .with_context(|| format!("subscribe to Host '{host}' events"))
                .map(|subscription| (index, subscription))
        });
    }
    let mut subscriptions = (0..connections.len()).map(|_| None).collect::<Vec<_>>();
    while let Some(joined) = jobs.join_next().await {
        let (index, subscription) = joined.context("Host event subscription task failed")??;
        subscriptions[index] = Some(subscription);
    }
    subscriptions
        .into_iter()
        .map(|subscription| subscription.ok_or_else(|| anyhow!("missing Host event subscription")))
        .collect()
}

/// Resolve a catalog entry on every Host or import one exact Wasm byte buffer
/// on every Host.  The first Host's metadata is the comparison baseline.
async fn resolve_program(
    hosts: &[HostConnection],
    reference: &str,
    progress: &RunProgress,
) -> anyhow::Result<ProgramDetail> {
    let wasm = wasm_reference(reference)
        .map(|path| {
            std::fs::read(path).with_context(|| format!("read Wasm program {}", path.display()))
        })
        .transpose()?;

    let mut jobs = JoinSet::new();
    for (index, host) in hosts.iter().enumerate() {
        let client = host.client.clone();
        let host_name = host.host.clone();
        let reference = reference.to_owned();
        let wasm = wasm.clone();
        jobs.spawn(async move {
            let response = if let Some(wasm) = wasm {
                client
                    .call_host(&host_name, &HostRequest::ProgramImport { wasm })
                    .await
            } else {
                client
                    .call_host(&host_name, &HostRequest::ProgramGet { program: reference })
                    .await
            }?;
            let ResponseOk::Program(detail) = response else {
                bail!("unexpected program response from Host '{host_name}': {response:?}");
            };
            Ok::<_, anyhow::Error>((index, *detail))
        });
    }

    let mut resolved = (0..hosts.len())
        .map(|_| None)
        .collect::<Vec<Option<ProgramDetail>>>();
    while let Some(joined) = jobs.join_next().await {
        progress.advance();
        let (index, detail) = joined.context("program resolution task failed to join")??;
        resolved[index] = Some(detail);
    }

    let mut resolved = resolved
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            value.ok_or_else(|| anyhow!("program resolution for Host {index} missing"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let baseline = resolved
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("coordinated run has no Hosts"))?;
    for (index, detail) in resolved.drain(..).enumerate().skip(1) {
        if detail != baseline {
            bail!(
                "program '{}' differs on Host '{}': expected hash {}, found {}",
                reference,
                hosts[index].host,
                baseline.summary.program_hash,
                detail.summary.program_hash
            );
        }
    }

    let abi = hosts[0].abi_version;
    if let Some((_index, host)) = hosts
        .iter()
        .enumerate()
        .find(|(_, host)| host.abi_version != abi)
    {
        bail!(
            "Host '{}' uses ABI {}, expected ABI {} for the coordinated run",
            host.host,
            host.abi_version,
            abi
        );
    }
    Ok(baseline)
}

pub(crate) fn wasm_reference(reference: &str) -> Option<&Path> {
    let path = Path::new(reference);
    (path.is_file()
        || path
            .extension()
            .is_some_and(|extension| extension == "wasm"))
    .then_some(path)
}

fn validate_participant_count(summary: &ProgramSummary, host_count: usize) -> anyhow::Result<()> {
    let host_count =
        u16::try_from(host_count).context("too many Hosts for protocol participant count")?;
    if !summary.participants.accepts(host_count) {
        bail!(
            "program '{}' accepts {} participants, but {} Hosts were selected",
            summary.name,
            summary.participants,
            host_count
        );
    }
    Ok(())
}

async fn create_executions(
    hosts: &[HostConnection],
    program_id: ProgramHash,
    params: Option<Value>,
    cancelled: &mut watch::Receiver<Option<String>>,
    progress: &RunProgress,
) -> anyhow::Result<Vec<LocalParticipant>> {
    if let Some(reason) = cancelled.borrow().clone() {
        bail!(reason);
    }
    let creator = hosts
        .first()
        .ok_or_else(|| anyhow!("cannot create a coordinated run with no Hosts"))?;
    let mut exec_ids = vec![None; hosts.len()];
    let creator_exec = ExecId(rand::random());
    exec_ids[0] = Some(creator_exec);
    let peers = hosts.iter().skip(1).map(|host| host.peer_id).collect();
    let created = call_during_creation(
        &creator.client,
        &creator.host,
        HostRequest::ExecNew {
            exec_id: creator_exec,
            program: program_id.to_string(),
            params,
            ensemble: EnsembleSpec::Explicit { peers },
        },
        cancelled,
    )
    .await;
    progress.advance();
    let negotiation_id = match created {
        Ok(ResponseOk::ExecCreated {
            exec_id,
            negotiation_id: Some(negotiation_id),
            ..
        }) if exec_id == creator_exec => negotiation_id,
        Ok(ResponseOk::ExecCreated { exec_id, .. }) => {
            let error = if exec_id == creator_exec {
                anyhow!("creator Host returned no negotiation id")
            } else {
                anyhow!("creator Host returned ExecId {exec_id}, requested {creator_exec}")
            };
            let partial = LocalParticipant {
                host: creator.host.clone(),
                client: creator.client.clone(),
                peer_id: creator.peer_id,
                exec_id: creator_exec,
                driver: creator.driver.clone(),
            };
            return match stop_participants(&[partial]).await {
                Ok(()) => {
                    Err(error.context(format!("requested execution {creator_exec} was cleaned up")))
                }
                Err(cleanup) => Err(error.context(format!(
                    "cleanup for requested execution {creator_exec} was incomplete: {cleanup:#}"
                ))),
            };
        }
        Ok(other) => bail!("unexpected creator exec.new response: {other:?}"),
        Err(error) => {
            let partial = LocalParticipant {
                host: creator.host.clone(),
                client: creator.client.clone(),
                peer_id: creator.peer_id,
                exec_id: creator_exec,
                driver: creator.driver.clone(),
            };
            return match stop_participants(&[partial]).await {
                Ok(()) => Err(error.context(format!(
                    "creator exec.new failed; requested execution {creator_exec} was cleaned up"
                ))),
                Err(cleanup) => Err(error.context(format!(
                    "creator exec.new failed; cleanup for requested execution {creator_exec} was incomplete: {cleanup:#}"
                ))),
            };
        }
    };

    let mut jobs = JoinSet::new();
    for (index, host) in hosts.iter().enumerate().skip(1) {
        if cancelled.borrow().is_some() {
            break;
        }
        let client = host.client.clone();
        let host_name = host.host.clone();
        let creator_peer = creator.peer_id;
        let exec_id = ExecId(rand::random());
        exec_ids[index] = Some(exec_id);
        let mut cancelled = cancelled.clone();
        jobs.spawn(async move {
            let result = async {
                let response = call_during_creation(
                    &client,
                    &host_name,
                    HostRequest::ExecNew {
                        exec_id,
                        program: program_id.to_string(),
                        params: None,
                        ensemble: EnsembleSpec::Join {
                            target: Some(arena0_client::protocol::NegotiationTarget::new(
                                creator_peer,
                                negotiation_id,
                            )),
                        },
                    },
                    &mut cancelled,
                )
                .await?;
                let ResponseOk::ExecCreated {
                    exec_id: returned, ..
                } = response
                else {
                    bail!("unexpected joiner exec.new response: {response:?}");
                };
                if returned != exec_id {
                    bail!("joiner Host returned ExecId {returned}, requested {exec_id}");
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            (index, result)
        });
    }

    let mut failures = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        progress.advance();
        match joined {
            Ok((_index, Ok(()))) => {}
            Ok((index, Err(error))) => failures.push(format!(
                "Host '{}' could not join: {error:#}",
                hosts[index].host
            )),
            Err(error) => failures.push(format!("joiner exec.new task failed to join: {error}")),
        }
    }

    if let Some(reason) = cancelled.borrow().clone() {
        failures.push(reason);
    }

    let mut distinct = HashSet::with_capacity(exec_ids.len());
    let mut participants = Vec::with_capacity(hosts.len());
    for (host, exec_id) in hosts.iter().zip(exec_ids) {
        let Some(exec_id) = exec_id else {
            continue;
        };
        if !distinct.insert(exec_id) {
            failures.push(format!(
                "two selected Hosts returned the same ExecId {exec_id}"
            ));
        }
        participants.push(LocalParticipant {
            host: host.host.clone(),
            client: host.client.clone(),
            peer_id: host.peer_id,
            exec_id,
            driver: host.driver.clone(),
        });
    }
    if !failures.is_empty() {
        let partial = Coordinator {
            participants,
            progress: progress.clone(),
        };
        let executions = partial.execution_refs();
        let cleanup = partial.stop_all().await;
        let failure = failures.join("; ");
        return match cleanup {
            Ok(()) => Err(anyhow!(
                "coordinated execution creation failed ({failure}); withdrawn executions: {executions}"
            )),
            Err(cleanup) => Err(anyhow!(
                "coordinated execution creation failed ({failure}); cleanup was incomplete ({cleanup:#}); durable executions: {executions}"
            )),
        };
    }
    Ok(participants)
}

async fn call_during_creation(
    client: &DaemonClient,
    host: &HostName,
    request: HostRequest,
    cancelled: &mut watch::Receiver<Option<String>>,
) -> anyhow::Result<ResponseOk> {
    let call = tokio::time::timeout(CREATE_REQUEST_TIMEOUT, client.call_host(host, &request));
    tokio::pin!(call);
    tokio::select! {
        result = &mut call => result
            .map_err(|_| anyhow!("timed out waiting for exec.new response"))?,
        _reason = wait_for_cancel_reason(cancelled) => {
            tokio::time::timeout(CANCELLED_CREATE_DRAIN_TIMEOUT, &mut call)
                .await
                .map_err(|_| anyhow!("timed out draining exec.new after cancellation"))?
                .map_err(|_| anyhow!("timed out waiting for exec.new response"))?
        }
    }
}

async fn await_activation(
    participants: &[LocalParticipant],
    progress: &RunProgress,
) -> anyhow::Result<SessionHash> {
    let mut jobs = JoinSet::new();
    for (index, participant) in participants.iter().enumerate() {
        let client = participant.client.clone();
        let host = participant.host.clone();
        let exec_id = participant.exec_id;
        jobs.spawn(async move {
            match client
                .call_host(
                    &host,
                    &HostRequest::ExecAwait {
                        exec_id,
                        until: AwaitState::Active,
                    },
                )
                .await
                .with_context(|| {
                    format!("wait for Host '{host}' execution {exec_id} to activate")
                })? {
                ResponseOk::Awaited {
                    exec_state: ExecLifecycle::Active,
                    ..
                } => {}
                ResponseOk::Awaited {
                    exec_state, reason, ..
                } => {
                    bail!(
                        "Host '{host}' execution {exec_id} did not activate ({exec_state:?}): {}",
                        reason.unwrap_or_else(|| "no reason supplied".to_owned())
                    );
                }
                other => bail!("unexpected exec.await response from Host '{host}': {other:?}"),
            }
            let ResponseOk::Status(status) = client
                .call_host(&host, &HostRequest::ExecStatus { exec_id })
                .await
                .with_context(|| {
                    format!("read Host '{host}' execution {exec_id} after activation")
                })?
            else {
                bail!("unexpected exec.status response from Host '{host}'");
            };
            if status.lifecycle() != ExecLifecycle::Active {
                bail!(
                    "Host '{host}' execution {exec_id} is {:?} after activation wait",
                    status.lifecycle()
                );
            }
            let session_id = status
                .session_id()
                .ok_or_else(|| anyhow!("Host '{host}' active execution has no SessionHash"))?;
            Ok::<_, anyhow::Error>((index, session_id))
        });
    }

    let mut session_ids = vec![None; participants.len()];
    while let Some(joined) = jobs.join_next().await {
        progress.advance();
        let (index, session_id) = joined.context("activation task failed to join")??;
        session_ids[index] = Some(session_id);
    }
    let expected = session_ids
        .first()
        .and_then(|id| *id)
        .ok_or_else(|| anyhow!("creator activation returned no SessionHash"))?;
    for (index, session_id) in session_ids.into_iter().enumerate() {
        let session_id = session_id.ok_or_else(|| anyhow!("Host {index} activation missing"))?;
        if session_id != expected {
            bail!("Host {index} activated SessionHash {session_id}, expected {expected}");
        }
    }
    Ok(expected)
}

async fn drive_to_terminal(
    host: HostName,
    client: DaemonClient,
    exec_id: ExecId,
    driver: DriverSpec,
    mut cancelled: watch::Receiver<bool>,
    tui: Option<TuiHandle>,
    progress: RunProgress,
) -> anyhow::Result<HostTerminal> {
    if driver == DriverSpec::External {
        let await_terminal = async {
            loop {
                match client
                    .call_host_raw(
                        &host,
                        &HostRequest::ExecAwait {
                            exec_id,
                            until: AwaitState::Terminal,
                        },
                    )
                    .await?
                {
                    Ok(ResponseOk::Awaited { .. }) => break,
                    Err(error) if error.code == ApiErrorCode::Timeout => continue,
                    Err(error) => return Err(error.into()),
                    other => bail!("unexpected exec.await response: {other:?}"),
                }
            }
            match client
                .call_host(&host, &HostRequest::ExecNext { exec_id })
                .await?
            {
                ResponseOk::Next(NextEvent::Completed {
                    session_id,
                    outcome,
                }) => Ok(HostTerminal::Completed {
                    session_id,
                    outcome,
                }),
                ResponseOk::Next(NextEvent::Failed { reason }) => {
                    Ok(HostTerminal::Failed { reason })
                }
                other => bail!("unexpected terminal exec.next response: {other:?}"),
            }
        };
        return tokio::select! {
            result = await_terminal => result,
            () = wait_for_cancel(&mut cancelled) => bail!("coordinated run cancelled while observing {exec_id}"),
        };
    }
    let mut driver = ActiveDriver::start(driver, tui.clone())?;
    let result = drive_loop(
        &host,
        &client,
        exec_id,
        &mut driver,
        &mut cancelled,
        tui,
        &progress,
    )
    .await;
    let cleanup = if result.is_ok() {
        driver.finish().await
    } else {
        driver.abort().await
    };
    match (result, cleanup) {
        (Ok(terminal), Ok(())) => Ok(terminal),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup.context("close executable driver after terminal")),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("executable-driver cleanup failed: {cleanup:#}")))
        }
    }
}

async fn drive_loop(
    host: &HostName,
    client: &DaemonClient,
    exec_id: ExecId,
    driver: &mut ActiveDriver,
    cancelled: &mut watch::Receiver<bool>,
    tui: Option<TuiHandle>,
    progress: &RunProgress,
) -> anyhow::Result<HostTerminal> {
    loop {
        let request = HostRequest::ExecNext { exec_id };
        let next = tokio::select! {
            response = client.call_host(host, &request) => response?,
            () = wait_for_cancel(cancelled) => {
                bail!("coordinated run cancelled while driving {exec_id}")
            }
        };
        match next {
            ResponseOk::Next(NextEvent::Callout {
                pending_id,
                callout_index,
                name,
                prompt,
                schema,
                context,
                ..
            }) => {
                let answer = driver.answer(
                    DriverAnswerContext {
                        host,
                        client,
                        exec_id,
                    },
                    pending_id,
                    callout_index,
                    Callout {
                        name: &name,
                        prompt: &prompt,
                        context: &context,
                        schema: schema.as_value(),
                    },
                    progress,
                );
                tokio::pin!(answer);
                let answer = tokio::select! {
                    answer = &mut answer => answer?,
                    () = wait_for_cancel(cancelled) => {
                        bail!("coordinated run cancelled while waiting for driver {exec_id}")
                    }
                };
                let submit = async {
                    match client
                        .call_host_raw(
                            host,
                            &HostRequest::ExecSubmit {
                                exec_id,
                                pending_id,
                                answer: Some(answer),
                            },
                        )
                        .await?
                    {
                        Ok(ResponseOk::Ack) => Ok(()),
                        Err(error) if error.code == ApiErrorCode::CalloutNotPending => Ok(()),
                        Err(error) => Err(error.into()),
                        other => bail!("unexpected exec.submit response: {other:?}"),
                    }
                };
                tokio::select! {
                    result = submit => result?,
                    () = wait_for_cancel(cancelled) => {
                        bail!("coordinated run cancelled while submitting driver answer for {exec_id}")
                    }
                }
            }
            ResponseOk::Next(NextEvent::Completed {
                session_id,
                outcome,
            }) => {
                if let Some(tui) = &tui {
                    tokio::try_join!(
                        refresh_tui(tui, host, client, exec_id),
                        refresh_tui_view(tui, host, client, exec_id),
                    )?;
                }
                return Ok(HostTerminal::Completed {
                    session_id,
                    outcome,
                });
            }
            ResponseOk::Next(NextEvent::Failed { reason }) => {
                return Ok(HostTerminal::Failed { reason });
            }
            other => bail!("unexpected exec.next response: {other:?}"),
        }
    }
}

struct TuiEventSource {
    host: HostName,
    client: DaemonClient,
    exec_id: ExecId,
    subscription: Subscription,
}

impl TuiEventSource {
    async fn observe(mut self, tui: &TuiHandle) -> anyhow::Result<()> {
        loop {
            let frame = self
                .subscription
                .next()
                .await?
                .ok_or_else(|| anyhow!("run TUI event stream closed"))?;
            if frame.exec_id == Some(self.exec_id) {
                let agreement = match &frame.data {
                    EventData::SessionStep {
                        signers,
                        participants,
                        ..
                    } => Some((*signers, *participants)),
                    _ => None,
                };
                let refresh_view = matches!(
                    &frame.data,
                    EventData::SessionStarted { .. }
                        | EventData::SessionCallout { .. }
                        | EventData::SessionCalloutAnswered { .. }
                        | EventData::SessionStep { .. }
                        | EventData::SessionEnded { .. }
                );
                tui.update(RunUpdate::SystemEvent { frame }).await?;
                if let Some((signers, participants)) = agreement {
                    tui.update(RunUpdate::Agreement {
                        host: self.host.clone(),
                        agreed: signers,
                        total: participants,
                    })
                    .await?;
                }
                if refresh_view {
                    tokio::try_join!(
                        refresh_tui(tui, &self.host, &self.client, self.exec_id),
                        refresh_tui_view(tui, &self.host, &self.client, self.exec_id),
                    )?;
                }
            } else if matches!(
                frame.data,
                EventData::HostStarted { .. }
                    | EventData::HostStopped { .. }
                    | EventData::OfferSeen { .. }
                    | EventData::Lagged { .. }
            ) {
                let refresh = matches!(&frame.data, EventData::Lagged { .. });
                tui.update(RunUpdate::SystemEvent { frame }).await?;
                if refresh {
                    tokio::try_join!(
                        refresh_tui(tui, &self.host, &self.client, self.exec_id),
                        refresh_tui_view(tui, &self.host, &self.client, self.exec_id),
                    )?;
                }
            }
        }
    }
}

async fn observe_tui(
    sources: Vec<TuiEventSource>,
    tui: TuiHandle,
    mut stopped: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut observers = JoinSet::new();
    let inspection_sources = sources
        .iter()
        .map(|source| (source.host.clone(), source.client.clone(), source.exec_id))
        .collect::<Vec<_>>();
    refresh_tui_all(&tui, &inspection_sources).await?;
    let view_sources = inspection_sources.clone();
    for source in sources {
        let events_tui = tui.clone();
        observers.spawn(async move { source.observe(&events_tui).await });
    }
    let inspection_tui = tui.clone();
    let inspection_page_sources = inspection_sources.clone();
    let width_tui = tui.clone();
    observers.spawn(async move {
        let mut tui = width_tui;
        loop {
            tui.changed_width().await;
            refresh_tui_views(&tui, &view_sources).await?;
        }
    });
    observers
        .spawn(async move { inspection::observe(inspection_tui, inspection_page_sources).await });

    tokio::select! {
        () = wait_for_cancel(&mut stopped) => {},
        result = observers.join_next() => match result {
            Some(result) => {
                result.context("run TUI observer task failed to join")??;
                bail!("run TUI observer stopped unexpectedly")
            }
            None => bail!("run TUI has no active observers"),
        },
    }
    observers.abort_all();
    while observers.join_next().await.is_some() {}
    refresh_tui_inspections(&tui, &inspection_sources).await;
    Ok(())
}

async fn refresh_tui(
    tui: &TuiHandle,
    host: &HostName,
    client: &DaemonClient,
    exec_id: ExecId,
) -> anyhow::Result<()> {
    let status = match client
        .call_host(host, &HostRequest::ExecStatus { exec_id })
        .await
        .with_context(|| format!("refresh TUI status for execution {exec_id}"))?
    {
        ResponseOk::Status(status) => status,
        other => bail!("unexpected exec.status response while refreshing TUI: {other:?}"),
    };
    tui.update(RunUpdate::Status {
        host: host.clone(),
        status: status.clone(),
    })
    .await?;
    if status.session().is_none() {
        return Ok(());
    }

    match client
        .call_host(
            host,
            &HostRequest::ExecTrace {
                exec_id,
                from: 0,
                to: u64::MAX,
            },
        )
        .await
        .with_context(|| format!("refresh TUI trace for execution {exec_id}"))?
    {
        ResponseOk::Trace(entries) => {
            tui.update(RunUpdate::Trace {
                host: host.clone(),
                entries,
            })
            .await?
        }
        other => bail!("unexpected exec.trace response while refreshing TUI: {other:?}"),
    }

    Ok(())
}

async fn refresh_tui_inspection(
    tui: &TuiHandle,
    host: &HostName,
    client: &DaemonClient,
    exec_id: ExecId,
    private_from: Option<u64>,
) -> anyhow::Result<()> {
    match client
        .call_host(
            host,
            &HostRequest::ExecInspect {
                exec_id,
                private_from,
                private_limit: PRIVATE_INSPECTION_LIMIT,
            },
        )
        .await
        .with_context(|| format!("refresh TUI inspection for Host '{host}' execution {exec_id}"))?
    {
        ResponseOk::Inspection(inspection) => {
            tui.update(RunUpdate::Inspection {
                host: host.clone(),
                inspection: Box::new(inspection),
            })
            .await
        }
        other => bail!(
            "unexpected exec.inspect response while refreshing TUI for Host '{host}': {other:?}"
        ),
    }
}

async fn refresh_tui_all(
    tui: &TuiHandle,
    sources: &[(HostName, DaemonClient, ExecId)],
) -> anyhow::Result<()> {
    let mut jobs = JoinSet::new();
    for (host, client, exec_id) in sources.iter().cloned() {
        let tui = tui.clone();
        jobs.spawn(async move {
            tokio::try_join!(
                refresh_tui(&tui, &host, &client, exec_id),
                refresh_tui_inspection(&tui, &host, &client, exec_id, None),
                refresh_tui_view(&tui, &host, &client, exec_id),
            )?;
            Ok::<_, anyhow::Error>(())
        });
    }
    while let Some(joined) = jobs.join_next().await {
        joined.context("TUI Host refresh task failed to join")??;
    }
    Ok(())
}

async fn refresh_tui_inspections(tui: &TuiHandle, sources: &[(HostName, DaemonClient, ExecId)]) {
    let mut jobs = JoinSet::new();
    for (host, client, exec_id) in sources.iter().cloned() {
        let tui = tui.clone();
        jobs.spawn(
            async move { refresh_tui_inspection(&tui, &host, &client, exec_id, None).await },
        );
    }
    while let Some(joined) = jobs.join_next().await {
        if joined.is_err() || joined.is_ok_and(|result| result.is_err()) {
            tracing::warn!("final TUI inspection refresh failed");
        }
    }
}

async fn refresh_tui_views(
    tui: &TuiHandle,
    sources: &[(HostName, DaemonClient, ExecId)],
) -> anyhow::Result<()> {
    let mut jobs = JoinSet::new();
    for (host, client, exec_id) in sources.iter().cloned() {
        let tui = tui.clone();
        jobs.spawn(async move { refresh_tui_view(&tui, &host, &client, exec_id).await });
    }
    while let Some(joined) = jobs.join_next().await {
        joined.context("TUI view refresh task failed to join")??;
    }
    Ok(())
}

async fn refresh_tui_view(
    tui: &TuiHandle,
    host: &HostName,
    client: &DaemonClient,
    exec_id: ExecId,
) -> anyhow::Result<()> {
    let Some((step, view)) = client
        .exec_view(
            host,
            exec_id,
            arena0_client::protocol::Viewport {
                width: tui.view_width(),
                color: ColorDepth::Mono,
            },
        )
        .await
        .with_context(|| format!("refresh TUI view for Host '{host}' execution {exec_id}"))?
    else {
        return Ok(());
    };
    tui.update(RunUpdate::View {
        host: host.clone(),
        step,
        view,
    })
    .await
}

#[derive(Debug)]
enum ActiveDriver {
    Human(Option<TuiHandle>),
    Builtin(String),
    Executable(Box<ExecutableAgent>),
}

impl ActiveDriver {
    fn start(spec: DriverSpec, tui: Option<TuiHandle>) -> anyhow::Result<Self> {
        match spec {
            DriverSpec::External => bail!("external executions do not own a local driver"),
            DriverSpec::Human => Ok(Self::Human(tui)),
            DriverSpec::Builtin(strategy) => Ok(Self::Builtin(strategy)),
            DriverSpec::Executable(path) => Ok(Self::Executable(Box::new(ExecutableAgent::spawn(
                path,
                DEFAULT_RESPONSE_TIMEOUT,
            )?))),
        }
    }

    async fn answer(
        &mut self,
        scope: DriverAnswerContext<'_>,
        pending_id: PendingId,
        callout_index: u32,
        callout: Callout<'_>,
        progress: &RunProgress,
    ) -> anyhow::Result<Value> {
        match self {
            Self::Human(Some(tui)) => {
                tui.answer(TuiCalloutRequest {
                    host: scope.host.clone(),
                    exec_id: scope.exec_id,
                    pending_id,
                    callout_index,
                    name: callout.name.to_owned(),
                    prompt: callout.prompt.to_owned(),
                    context: callout.context.clone(),
                    schema: callout.schema.clone(),
                })
                .await
            }
            Self::Human(None) => {
                progress.suspend_for_callout();
                let answer = prompt_human(
                    scope.host,
                    scope.client,
                    scope.exec_id,
                    callout.name,
                    callout.prompt,
                    callout.context,
                    callout.schema,
                )
                .await;
                if answer.is_ok() {
                    progress.resume_execution();
                }
                answer
            }
            Self::Builtin(strategy) => deterministic_builtin_answer(
                strategy,
                callout.name,
                callout.context,
                callout.schema,
            ),
            Self::Executable(agent) => {
                agent
                    .answer(
                        callout.name,
                        callout.prompt,
                        callout.context,
                        callout.schema,
                    )
                    .await
            }
        }
    }

    async fn finish(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Human(_) | Self::Builtin(_) => Ok(()),
            Self::Executable(agent) => agent.finish().await,
        }
    }

    async fn abort(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Human(_) | Self::Builtin(_) => Ok(()),
            Self::Executable(agent) => agent.terminate().await,
        }
    }
}

async fn wait_for_cancel(cancelled: &mut watch::Receiver<bool>) {
    if *cancelled.borrow() {
        return;
    }
    while cancelled.changed().await.is_ok() {
        if *cancelled.borrow() {
            return;
        }
    }
}

async fn wait_for_cancel_reason(cancelled: &mut watch::Receiver<Option<String>>) -> String {
    if let Some(reason) = cancelled.borrow().clone() {
        return reason;
    }
    while cancelled.changed().await.is_ok() {
        if let Some(reason) = cancelled.borrow().clone() {
            return reason;
        }
    }
    "run TUI closed unexpectedly".to_owned()
}

async fn prompt_human(
    host: &HostName,
    client: &DaemonClient,
    exec_id: ExecId,
    name: &str,
    prompt: &str,
    context: &Value,
    schema: &Value,
) -> anyhow::Result<Value> {
    let palette = crate::ui::Palette::for_stderr(crate::ui::Mode::Human);
    if let Some(view) = fetch_human_view(host, client, exec_id, palette.color_depth()).await? {
        let rendered = crate::ui::render_view_summary(&view, palette);
        if !rendered.is_empty() {
            eprintln!();
            eprint!("{rendered}");
        }
    }
    eprintln!();
    eprintln!("callout  {name}  {prompt}");
    if !context.is_null() {
        eprintln!("  context: {}", crate::ui::compact_json(context));
    }
    eprintln!("  schema:");
    eprintln!(
        "{}",
        serde_json::to_string_pretty(schema).context("render callout answer schema")?
    );
    eprint!("> ");
    std::io::stderr().flush().ok();
    read_validated_answer(schema, crate::line_input::read_line).await
}

async fn fetch_human_view(
    host: &HostName,
    client: &DaemonClient,
    exec_id: ExecId,
    color: ColorDepth,
) -> anyhow::Result<Option<View>> {
    client
        .exec_view(
            host,
            exec_id,
            arena0_client::protocol::Viewport {
                width: crate::ui::terminal_width(),
                color,
            },
        )
        .await
        .map(|view| view.map(|(_, view)| view))
        .context("fetch execution view while preparing callout")
}

pub(crate) async fn read_validated_answer<F, Fut>(
    schema: &Value,
    mut read_line: F,
) -> anyhow::Result<Value>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Option<String>>>,
{
    let validator = jsonschema::validator_for(schema).context("compile callout answer schema")?;
    loop {
        let line = read_line()
            .await
            .context("read callout answer")?
            .ok_or_else(|| anyhow!("stdin closed before the callout was answered"))?;
        let value = answer::scalar(line.trim());
        match validator.validate(&value) {
            Ok(()) => return Ok(value),
            Err(error) => {
                let path = error.instance_path().as_str();
                let path = if path.is_empty() { "$" } else { path };
                eprintln!("invalid answer at {path}: {error}");
                eprint!("> ");
                std::io::stderr().flush().ok();
            }
        }
    }
}

async fn verify_one_receipt(
    host: HostName,
    client: DaemonClient,
    session_id: SessionHash,
    peer_id: PeerId,
    tier: VerificationTier,
) -> anyhow::Result<HostEvidence> {
    let mut last_error = None;
    for attempt in 0..RECEIPT_RETRY_ATTEMPTS {
        match client
            .call_host_raw(
                &host,
                &HostRequest::ReceiptVerify {
                    receipt: ReceiptRef::Produced(session_id),
                    full: tier.is_full(),
                },
            )
            .await?
        {
            Ok(ResponseOk::Verified {
                receipt_id,
                program_id,
                session_id,
                ensemble,
                steps,
                result,
            }) => {
                return Ok(HostEvidence {
                    receipt_id,
                    peer_id,
                    program_id,
                    session_id,
                    ensemble,
                    steps,
                    result,
                });
            }
            Ok(other) => bail!("unexpected receipt.verify response: {other:?}"),
            Err(error) if error.code == ApiErrorCode::NotFound => {
                last_error = Some(error);
                if attempt + 1 < RECEIPT_RETRY_ATTEMPTS {
                    tokio::time::sleep(RECEIPT_RETRY_DELAY).await;
                }
            }
            Err(error) => bail!("receipt verification failed: {error}"),
        }
    }
    match last_error {
        Some(error) => Err(anyhow!(error)),
        None => bail!("receipt verification never completed"),
    }
}

fn compare_terminals(terminals: &[HostTerminal]) -> anyhow::Result<TerminalConsensus> {
    let Some(first) = terminals.first() else {
        bail!("coordinated run produced no terminal results");
    };
    let consensus = match first {
        HostTerminal::Completed {
            session_id,
            outcome,
        } => TerminalConsensus::Completed {
            session_id: *session_id,
            outcome: outcome.clone(),
        },
        HostTerminal::Failed { .. } => TerminalConsensus::Stopped,
    };
    for terminal in &terminals[1..] {
        match (&consensus, terminal) {
            (
                TerminalConsensus::Completed {
                    session_id,
                    outcome,
                },
                HostTerminal::Completed {
                    session_id: other_session,
                    outcome: other_outcome,
                },
            ) if other_session == session_id && other_outcome == outcome => {}
            (
                TerminalConsensus::Completed { session_id, .. },
                HostTerminal::Completed {
                    session_id: other_session,
                    ..
                },
            ) => bail!(
                "Hosts returned incompatible terminals: session {session_id} vs {other_session}"
            ),
            (TerminalConsensus::Completed { .. }, HostTerminal::Failed { reason }) => {
                bail!("one Host stopped while others completed: {reason}")
            }
            (TerminalConsensus::Stopped, HostTerminal::Failed { .. }) => {}
            (TerminalConsensus::Stopped, HostTerminal::Completed { .. }) => {
                bail!("one Host completed while others stopped")
            }
        }
    }
    Ok(consensus)
}

/// Compare the facts every Host receipt claims.  This is intentionally a
/// pure function so disagreement remains easy to test without a daemon.
pub(crate) fn compare_evidence(receipts: &[HostEvidence]) -> anyhow::Result<EvidenceAgreement> {
    let Some(first) = receipts.first() else {
        bail!("no Host receipts were verified");
    };
    for receipt in &receipts[1..] {
        if receipt.receipt_id != first.receipt_id {
            bail!("Hosts retained different receipt artifacts");
        }
        if receipt.program_id != first.program_id {
            bail!("Host receipts disagree on ProgramHash");
        }
        if receipt.session_id != first.session_id {
            bail!("Host receipts disagree on SessionHash");
        }
        if receipt.ensemble != first.ensemble {
            bail!("Host receipts disagree on ordered participant ensemble");
        }
        if receipt.steps != first.steps {
            bail!("Host receipts disagree on step count");
        }
        if receipt.result != first.result {
            bail!("Host receipts disagree on terminal result");
        }
    }
    Ok(EvidenceAgreement {
        receipt_id: first.receipt_id,
        program_id: first.program_id,
        session_id: first.session_id,
        ensemble: first.ensemble.clone(),
        steps: first.steps,
        result: first.result.clone(),
    })
}

fn bind_verified_terminal(
    live: TerminalConsensus,
    verified: &VerifiedResult,
) -> anyhow::Result<AggregateTerminal> {
    match (live, verified) {
        (
            TerminalConsensus::Completed { outcome: live, .. },
            VerifiedResult::Full {
                terminal: arena0_client::api::FullVerifiedTerminal::Completed { outcome_json, .. },
            },
        ) => {
            if live.as_ref() != Some(outcome_json) {
                bail!("live terminal outcome disagrees with fully replayed receipt outcome");
            }
            Ok(AggregateTerminal::Completed {
                outcome: Some(outcome_json.clone()),
            })
        }
        (
            TerminalConsensus::Completed { outcome, .. },
            VerifiedResult::Light {
                terminal: arena0_client::api::LightVerifiedTerminal::Completed { .. },
            },
        ) => Ok(AggregateTerminal::Completed { outcome }),
        (
            TerminalConsensus::Stopped,
            VerifiedResult::Light {
                terminal: arena0_client::api::LightVerifiedTerminal::Stopped { cause },
            }
            | VerifiedResult::Full {
                terminal: arena0_client::api::FullVerifiedTerminal::Stopped { cause },
            },
        ) => Ok(classify_stop(cause)),
        (TerminalConsensus::Completed { .. }, _) => {
            bail!("live completion disagrees with stopped receipt evidence")
        }
        (TerminalConsensus::Stopped, _) => {
            bail!("live stop disagrees with completed receipt evidence")
        }
    }
}

fn classify_stop(cause: &StopCause) -> AggregateTerminal {
    match cause.kind() {
        AbortKind::Abort => AggregateTerminal::Stopped,
        AbortKind::Fail => AggregateTerminal::Failed,
    }
}

/// Answer one callout with a named deterministic launch policy.
///
/// `sample` contains the small example policy for each bundled supported
/// program. `first-allowed` handles closed enum schemas directly. The Host
/// remains the authority for answer-schema validation.
pub(crate) fn deterministic_builtin_answer(
    strategy: &str,
    name: &str,
    context: &Value,
    schema: &Value,
) -> anyhow::Result<Value> {
    match strategy.trim().to_ascii_lowercase().as_str() {
        "first" | "first-allowed" => first_allowed_answer(schema),
        "sample" => sample_answer(name, context, schema),
        _ => bail!(
            "unknown built-in strategy '{strategy}'; supported strategies: first-allowed, sample"
        ),
    }
}

fn validate_builtin_strategy(strategy: &str) -> anyhow::Result<()> {
    match strategy.trim().to_ascii_lowercase().as_str() {
        "first" | "first-allowed" | "sample" => Ok(()),
        _ => bail!(
            "unknown built-in strategy '{strategy}'; supported strategies: first-allowed, sample"
        ),
    }
}

fn first_allowed_answer(schema: &Value) -> anyhow::Result<Value> {
    let mut references = HashSet::new();
    first_allowed_value(schema, schema, &mut references, 0).ok_or_else(|| {
        anyhow!("built-in first-allowed requires a callout schema with an allowed enum")
    })
}

fn sample_answer(name: &str, context: &Value, schema: &Value) -> anyhow::Result<Value> {
    match name {
        "MakeMove" => context
            .get("legal_moves")
            .and_then(Value::as_str)
            .and_then(|moves| moves.split(',').map(str::trim).find(|mv| !mv.is_empty()))
            .map(|mv| Value::String(mv.to_owned()))
            .ok_or_else(|| anyhow!("sample chess policy received no legal moves")),
        "SubmitBid" => Ok(Value::from(0)),
        "SubmitOffer" => sample_offer(context),
        _ => first_allowed_answer(schema).map_err(|_| {
            anyhow!("built-in sample has no policy for callout '{name}' and its schema has no allowed enum")
        }),
    }
}

fn sample_offer(context: &Value) -> anyhow::Result<Value> {
    let tasks = context
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("sample contract-net policy requires a tasks array"))?;
    let maximum_capacity = context
        .get("maximum_capacity")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("sample contract-net policy requires maximum_capacity"))?;
    let maximum_cost = context
        .get("maximum_cost")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("sample contract-net policy requires maximum_cost"))?;
    let capacity = maximum_capacity.min(tasks.len() as u64);
    let mut capabilities = Vec::new();
    let mut bids = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        let capability = task
            .get("capability")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("sample contract-net task {index} has no capability"))?;
        if !capabilities.iter().any(|known| known == capability) {
            capabilities.push(capability.to_owned());
        }
        bids.push(serde_json::json!({
            "task": index,
            "cost": (index as u64 + 1).min(maximum_cost),
        }));
    }
    Ok(serde_json::json!({
        "capabilities": capabilities,
        "capacity": capacity,
        "bids": bids,
    }))
}

fn first_allowed_value(
    node: &Value,
    root: &Value,
    references: &mut HashSet<String>,
    depth: usize,
) -> Option<Value> {
    if depth > 32 {
        return None;
    }
    let object = node.as_object()?;
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values.first().cloned();
    }
    if let Some(value) = object.get("const") {
        return Some(value.clone());
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let pointer = reference.strip_prefix('#')?;
        if !references.insert(reference.to_owned()) {
            return None;
        }
        return root
            .pointer(pointer)
            .and_then(|value| first_allowed_value(value, root, references, depth + 1));
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            for branch in branches {
                if let Some(value) = first_allowed_value(branch, root, references, depth + 1) {
                    return Some(value);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_client::api::{
        ApiError, ExecStatus, ExecStatusState, LightVerifiedTerminal, Response, SessionStatus,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncRead, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct TraceBuffer(Arc<Mutex<Vec<u8>>>);

    struct TraceWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for TraceWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("trace buffer")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for TraceBuffer {
        type Writer = TraceWriter;

        fn make_writer(&'a self) -> Self::Writer {
            TraceWriter(Arc::clone(&self.0))
        }
    }

    fn host(name: &str) -> HostName {
        name.parse().expect("valid Host name")
    }

    async fn read_host_request<R>(read: &mut R, expected_host: &HostName) -> HostRequest
    where
        R: AsyncRead + Unpin,
    {
        let envelope = arena0_client::api::frame::read_frame::<_, Request>(read)
            .await
            .expect("read routed Host request")
            .expect("Host request frame");
        let Request::Host { host, request } = envelope else {
            panic!("request was not routed through host.call");
        };
        assert_eq!(
            host,
            expected_host.to_string(),
            "request targeted wrong Host"
        );
        request
    }

    fn test_progress() -> RunProgress {
        RunProgress::new(
            crate::progress::ProgressMode::Hidden,
            crate::ui::Palette::plain(),
        )
    }

    fn schema(value: Value) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"Choice": value},
            "$ref": "#/$defs/Choice"
        })
    }

    #[tokio::test]
    async fn human_callout_view_fetches_the_program_owned_state() {
        let directory = tempfile::tempdir().expect("socket directory");
        let socket = directory.path().join("view.sock");
        let listener = UnixListener::bind(&socket).expect("bind scripted Host");
        let expected_host = host("host-01");
        let server_host = expected_host.clone();
        let exec_id = ExecId([0x11; 32]);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept exec.view");
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let request = read_host_request(&mut read, &server_host).await;
            assert!(matches!(
                request,
                HostRequest::ExecView {
                    exec,
                    color: ColorDepth::Mono,
                    ..
                } if exec == exec_id
            ));
            let response: Response = Ok(ResponseOk::ExecView {
                step: 4,
                view: View::new().state("score 1").status_bar("round 2"),
            });
            arena0_client::api::frame::write_frame(&mut write, &response)
                .await
                .expect("write exec.view");
        });

        let view = fetch_human_view(
            &expected_host,
            &DaemonClient::new(socket),
            exec_id,
            ColorDepth::Mono,
        )
        .await
        .expect("fetch program view")
        .expect("program view");
        assert_eq!(
            view.slots.get(&arena0_client::protocol::Slot::State),
            Some(&"score 1".to_owned())
        );
        assert_eq!(
            view.slots.get(&arena0_client::protocol::Slot::StatusBar),
            Some(&"round 2".to_owned())
        );
        server.await.expect("scripted Host");
    }

    #[tokio::test]
    async fn human_callout_view_ignores_the_existing_execution_error() {
        let directory = tempfile::tempdir().expect("socket directory");
        let socket = directory.path().join("view-error.sock");
        let listener = UnixListener::bind(&socket).expect("bind scripted Host");
        let expected_host = host("host-01");
        let server_host = expected_host.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept exec.view");
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let _request = read_host_request(&mut read, &server_host).await;
            let response: Response = Err(ApiError::new(
                ApiErrorCode::Execution,
                "view unavailable before activation",
            ));
            arena0_client::api::frame::write_frame(&mut write, &response)
                .await
                .expect("write exec.view error");
        });

        assert!(
            fetch_human_view(
                &expected_host,
                &DaemonClient::new(socket),
                ExecId([0x22; 32]),
                ColorDepth::Mono,
            )
            .await
            .expect("execution errors are a valid no-view result")
            .is_none()
        );
        server.await.expect("scripted Host");
    }

    #[tokio::test]
    async fn human_answer_validation_retries_without_submitting_invalid_value() {
        let schema = schema(json!({"enum": ["Cooperate", "Defect"]}));
        let mut lines = vec!["cooperate\n".to_owned(), "Cooperate\n".to_owned()].into_iter();
        let mut reads = 0;
        let answer = read_validated_answer(&schema, || {
            reads += 1;
            std::future::ready(Ok(lines.next()))
        })
        .await
        .expect("second answer is valid");

        assert_eq!(answer, json!("Cooperate"));
        assert_eq!(reads, 2, "invalid input must not be submitted");
    }

    #[tokio::test]
    async fn human_answer_validation_stops_at_eof() {
        let schema = schema(json!({"type": "string"}));
        let error = read_validated_answer(&schema, || std::future::ready(Ok(None)))
            .await
            .expect_err("EOF must stop the prompt");
        assert!(error.to_string().contains("stdin closed"));
    }

    #[test]
    fn performance_stage_fields_are_fixed_and_redacted() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(TraceBuffer(Arc::clone(&bytes)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            record_stage(
                "execute",
                Instant::now(),
                2,
                true,
                Some(ProgramHash([1; 32])),
                Some(ExecId([2; 32])),
                Some(SessionHash([3; 32])),
            );
        });

        let output =
            String::from_utf8(bytes.lock().expect("trace buffer").clone()).expect("UTF-8 trace");
        let event: Value = serde_json::from_str(output.trim()).expect("JSON trace");
        let fields = event["fields"].as_object().expect("trace fields");
        let mut names = fields.keys().map(String::as_str).collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "elapsed_us",
                "exec_id",
                "hosts",
                "message",
                "operation",
                "program_id",
                "session_id",
                "success",
            ]
        );
        let encoded = serde_json::to_string(fields).expect("encode trace fields");
        for forbidden in [
            "params",
            "answer",
            "context",
            "outcome",
            "receipt",
            "agent_path",
            "stderr",
        ] {
            assert!(!encoded.contains(forbidden), "leaked field {forbidden}");
        }
    }

    fn adapt_script_response(request: &HostRequest, response: &mut Response) {
        match (request, response) {
            (
                HostRequest::ExecNew { exec_id, .. },
                Ok(ResponseOk::ExecCreated {
                    exec_id: returned, ..
                }),
            ) if *returned == ExecId([0; 32]) => *returned = *exec_id,
            (
                HostRequest::ExecAwait { exec_id, .. },
                Ok(ResponseOk::Awaited {
                    exec_id: returned, ..
                }),
            ) if *returned == ExecId([0; 32]) => *returned = *exec_id,
            (HostRequest::ExecStatus { exec_id }, Ok(ResponseOk::Status(status)))
                if status.exec_id == ExecId([0; 32]) =>
            {
                status.exec_id = *exec_id
            }
            _ => {}
        }
    }

    async fn serve_script(
        listener: UnixListener,
        expected_host: HostName,
        responses: Vec<Response>,
    ) -> Vec<HostRequest> {
        let mut requests = Vec::with_capacity(responses.len());
        for mut response in responses {
            let (stream, _) = listener.accept().await.expect("accept scripted request");
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let envelope = arena0_client::api::frame::read_frame::<_, Request>(&mut read)
                .await
                .expect("read scripted request")
                .expect("request frame");
            let Request::Host { host, request } = envelope else {
                panic!("scripted request was not routed through host.call");
            };
            assert_eq!(
                host,
                expected_host.to_string(),
                "request targeted wrong Host"
            );
            adapt_script_response(&request, &mut response);
            requests.push(request);
            arena0_client::api::frame::write_frame(&mut write, &response)
                .await
                .expect("write scripted response");
        }
        requests
    }

    async fn serve_shared_script(
        listener: UnixListener,
        scripts: Vec<(HostName, Vec<Response>)>,
    ) -> Vec<(HostName, HostRequest)> {
        let mut scripts = scripts
            .into_iter()
            .map(|(host, responses)| (host.to_string(), (host, responses)))
            .collect::<BTreeMap<_, _>>();
        let mut remaining = scripts
            .values()
            .map(|(_, responses)| responses.len())
            .sum::<usize>();
        let mut requests = Vec::with_capacity(remaining);
        while remaining > 0 {
            let (stream, _) = listener.accept().await.expect("accept shared request");
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let envelope = arena0_client::api::frame::read_frame::<_, Request>(&mut read)
                .await
                .expect("read shared request")
                .expect("request frame");
            let Request::Host {
                host: wire_host,
                request,
            } = envelope
            else {
                panic!("shared request was not routed through host.call");
            };
            let Some((expected_host, responses)) = scripts.get_mut(&wire_host) else {
                panic!("request targeted unknown Host {wire_host}");
            };
            assert_eq!(
                wire_host,
                expected_host.to_string(),
                "request targeted the wrong Host"
            );
            assert!(
                !responses.is_empty(),
                "Host received more requests than scripted"
            );
            let mut response = responses.remove(0);
            adapt_script_response(&request, &mut response);
            requests.push((expected_host.clone(), request));
            arena0_client::api::frame::write_frame(&mut write, &response)
                .await
                .expect("write shared response");
            remaining -= 1;
        }
        requests
    }

    #[tokio::test]
    async fn driver_continues_after_another_client_answers_the_callout() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("answer-race.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let exec_id = ExecId([0x81; 32]);
        let session_id = SessionHash([0x82; 32]);
        let pending_id = PendingId::new(17);
        let server = tokio::spawn(serve_script(
            listener,
            host("host-01"),
            vec![
                Ok(ResponseOk::Next(NextEvent::Callout {
                    pending_id,
                    callout_index: 0,
                    name: "Decide".into(),
                    prompt: "Choose".into(),
                    schema: arena0_client::program::JsonSchemaDocument::new(schema(
                        json!({"type":"string", "enum":["yes"]}),
                    ))
                    .unwrap(),
                    context: Value::Null,
                })),
                Err(arena0_client::api::ApiError::new(
                    ApiErrorCode::CalloutNotPending,
                    "already answered",
                )),
                Ok(ResponseOk::Next(NextEvent::Completed {
                    session_id,
                    outcome: None,
                })),
            ],
        ));
        let (_cancel, cancelled) = watch::channel(false);
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            drive_to_terminal(
                host("host-01"),
                DaemonClient::new(socket),
                exec_id,
                DriverSpec::Builtin("first-allowed".into()),
                cancelled,
                None,
                test_progress(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            result,
            HostTerminal::Completed {
                session_id,
                outcome: None
            }
        );
        let requests = server.await.unwrap();
        assert!(
            matches!(requests.as_slice(), [HostRequest::ExecNext { .. }, HostRequest::ExecSubmit { pending_id: id, .. }, HostRequest::ExecNext { .. }] if *id == pending_id)
        );
    }

    #[tokio::test]
    async fn external_driver_waits_for_terminal_without_consuming_or_answering_callouts() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("external.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let exec_id = ExecId([0x83; 32]);
        let session_id = SessionHash([0x84; 32]);
        let server = tokio::spawn(serve_script(
            listener,
            host("host-01"),
            vec![
                Err(arena0_client::api::ApiError::new(
                    ApiErrorCode::Timeout,
                    "still active",
                )),
                Ok(ResponseOk::Awaited {
                    exec_id,
                    exec_state: ExecLifecycle::Completed,
                    reason: None,
                }),
                Ok(ResponseOk::Next(NextEvent::Completed {
                    session_id,
                    outcome: None,
                })),
            ],
        ));
        let (_cancel, cancelled) = watch::channel(false);
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            drive_to_terminal(
                host("host-01"),
                DaemonClient::new(socket),
                exec_id,
                DriverSpec::External,
                cancelled,
                None,
                test_progress(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            result,
            HostTerminal::Completed {
                session_id,
                outcome: None
            }
        );
        assert!(matches!(
            server.await.unwrap().as_slice(),
            [
                HostRequest::ExecAwait {
                    until: AwaitState::Terminal,
                    ..
                },
                HostRequest::ExecAwait {
                    until: AwaitState::Terminal,
                    ..
                },
                HostRequest::ExecNext { .. }
            ]
        ));
    }

    fn created_response(
        exec_id: ExecId,
        negotiation_id: arena0_client::protocol::NegotiationId,
    ) -> Response {
        Ok(ResponseOk::ExecCreated {
            exec_id,
            negotiation_id: Some(negotiation_id),
            session_id: None,
            exec_state: ExecLifecycle::Negotiating,
            queue_position: None,
        })
    }

    fn host_info_response(name: &str, peer: PeerId) -> Response {
        let status = serde_json::from_value(json!({
            "host": {
                "id": name,
                "peer_id": peer,
                "user_agent": "cli-test/1",
            },
            "transport_key": format!("{:02x}", peer.0[0]).repeat(32),
            "programs": 1,
            "execs_active": 0,
        }))
        .expect("scripted Host info");
        Ok(ResponseOk::HostStatus(status))
    }

    fn program_response(program_id: ProgramHash) -> Response {
        let schema = json!({"$schema": "https://json-schema.org/draft/2020-12/schema"});
        let detail = serde_json::from_value(json!({
            "summary": {
                "program_hash": program_id,
                "name": "scripted",
                "display_name": "Scripted",
                "version": "1.0.0",
                "description": "typed progress test",
                "participants": {"kind": "exact", "count": 2},
            },
            "schema": {
                "state": {"schema": schema, "max_bytes": 64},
                "callouts": [],
                "messages": [],
                "params": schema,
                "queries": [],
                "outcome": schema,
            },
        }))
        .expect("scripted program detail");
        Ok(ResponseOk::Program(Box::new(detail)))
    }

    fn active_status_response(
        program_id: ProgramHash,
        session_id: SessionHash,
        peer: PeerId,
    ) -> Response {
        Ok(ResponseOk::Status(ExecStatus {
            exec_id: ExecId([0; 32]),
            negotiation_id: Some(arena0_client::protocol::NegotiationId([0x42; 32])),
            program_id,
            state: ExecStatusState::Active {
                session: SessionStatus {
                    session_id,
                    step: 0,
                    peers: vec![peer],
                    participants: 2,
                    pending_callout: None,
                    receipt_available: false,
                },
            },
        }))
    }

    async fn run_scripted_coordinator(replay: bool) -> Vec<crate::progress::RunProgressState> {
        let directory = tempfile::tempdir().expect("socket directory");
        let program_id = ProgramHash([0x31; 32]);
        let session_id = SessionHash([0x32; 32]);
        let negotiation_id = arena0_client::protocol::NegotiationId([0x42; 32]);
        let peers = [PeerId([1; 32]), PeerId([2; 32])];
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind shared daemon");
        let client = DaemonClient::new(socket);
        let mut scripts = Vec::new();
        let mut connections = Vec::new();

        for (index, name) in ["first", "second"].into_iter().enumerate() {
            let terminal = if replay {
                VerifiedResult::Full {
                    terminal: arena0_client::api::FullVerifiedTerminal::Completed {
                        outcome_borsh: vec![1],
                        outcome_json: json!({"winner": "none"}),
                    },
                }
            } else {
                VerifiedResult::Light {
                    terminal: LightVerifiedTerminal::Completed {
                        outcome_borsh: vec![1],
                    },
                }
            };
            let responses = vec![
                host_info_response(name, peers[index]),
                program_response(program_id),
                created_response(ExecId([0; 32]), negotiation_id),
                Ok(ResponseOk::Awaited {
                    exec_id: ExecId([0; 32]),
                    exec_state: ExecLifecycle::Active,
                    reason: None,
                }),
                active_status_response(program_id, session_id, peers[1 - index]),
                Ok(ResponseOk::Next(NextEvent::Completed {
                    session_id,
                    outcome: Some(json!({"winner": "none"})),
                })),
                Ok(ResponseOk::Verified {
                    receipt_id: arena0_client::protocol::ReceiptId::from_bytes([9; 32]),
                    program_id,
                    session_id,
                    ensemble: peers.to_vec(),
                    steps: 3,
                    result: terminal,
                }),
            ];
            scripts.push((host(name), responses));
            connections.push(HostConnection {
                host: host(name),
                client: client.clone(),
                peer_id: peers[index],
                abi_version: arena0_client::protocol::ABI_VERSION,
                driver: DriverSpec::Builtin("sample".into()),
            });
        }
        let server = tokio::spawn(serve_shared_script(listener, scripts));

        let progress = test_progress();
        progress
            .during(RunStage::Connecting, connections.len(), async {
                for connection in &connections {
                    assert!(matches!(
                        connection
                            .client
                            .call_host(&connection.host, &HostRequest::Info)
                            .await
                            .expect("scripted Host info"),
                        ResponseOk::HostStatus(_)
                    ));
                    progress.advance();
                }
            })
            .await;
        let resolved = progress
            .during(
                RunStage::ProgramResolution,
                connections.len(),
                resolve_program(&connections, "scripted", &progress),
            )
            .await
            .expect("scripted program resolution");
        assert_eq!(resolved.summary.program_hash, program_id);

        let (_creation_cancel, mut creation_cancelled) = watch::channel(None);
        let participants = progress
            .during(
                RunStage::Negotiation,
                connections.len(),
                create_executions(
                    &connections,
                    program_id,
                    None,
                    &mut creation_cancelled,
                    &progress,
                ),
            )
            .await
            .expect("scripted negotiation");
        let coordinator = Coordinator {
            participants,
            progress: progress.clone(),
        };
        let activated = progress
            .during(
                RunStage::Activation,
                coordinator.participants.len(),
                await_activation(&coordinator.participants, &progress),
            )
            .await
            .expect("scripted activation");
        assert_eq!(activated, session_id);

        let (cancel, cancelled) = watch::channel(false);
        let terminals = progress
            .during(
                RunStage::Execution,
                0,
                coordinator.drive(cancel, cancelled, None),
            )
            .await
            .expect("scripted execution");
        assert_eq!(terminals.len(), 2);

        let tier = if replay {
            VerificationTier::Full
        } else {
            VerificationTier::Light
        };
        let receipt_stage = if replay {
            RunStage::Replay
        } else {
            RunStage::Verification
        };
        let receipts = progress
            .during(
                receipt_stage,
                coordinator.participants.len(),
                coordinator.verify_receipts(session_id, tier, None),
            )
            .await
            .expect("scripted receipt verification");
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            compare_evidence(&receipts).expect("shared evidence").steps,
            3
        );
        progress.terminal(RunTerminalState::Succeeded);
        let requests = server.await.expect("shared daemon script");
        assert_eq!(requests.len(), 14);
        for name in ["first", "second"] {
            let expected = host(name);
            assert_eq!(
                requests
                    .iter()
                    .filter(|(target, _)| target == &expected)
                    .count(),
                7,
                "shared script request count for Host '{name}'"
            );
        }
        progress.observations()
    }

    #[tokio::test]
    async fn coordinated_work_reports_real_typed_stages_and_exact_receipt_counts() {
        for (replay, receipt_stage) in [(false, RunStage::Verification), (true, RunStage::Replay)] {
            let observations = run_scripted_coordinator(replay).await;
            for stage in [
                RunStage::Connecting,
                RunStage::ProgramResolution,
                RunStage::Negotiation,
                RunStage::Activation,
                receipt_stage,
            ] {
                assert!(
                    observations.contains(&crate::progress::RunProgressState::Active {
                        stage,
                        amount: crate::progress::ProgressAmount::Known {
                            completed: 2,
                            total: 2,
                        },
                    })
                );
            }
            assert!(
                observations.contains(&crate::progress::RunProgressState::Active {
                    stage: RunStage::Execution,
                    amount: crate::progress::ProgressAmount::Indeterminate,
                })
            );
            assert_eq!(
                observations.last(),
                Some(&crate::progress::RunProgressState::Terminal(
                    RunTerminalState::Succeeded
                ))
            );
        }
    }

    #[tokio::test]
    async fn creator_id_mismatch_withdraws_the_requested_execution() {
        let directory = tempfile::tempdir().expect("socket directory");
        let socket = directory.path().join("creator.sock");
        let listener = UnixListener::bind(&socket).expect("bind scripted Host");
        let negotiation_id = arena0_client::protocol::NegotiationId([0x51; 32]);
        let returned_exec = ExecId([0x52; 32]);
        let server = tokio::spawn(serve_script(
            listener,
            host("creator"),
            vec![
                created_response(returned_exec, negotiation_id),
                Ok(ResponseOk::Ack),
            ],
        ));
        let hosts = vec![HostConnection {
            host: host("creator"),
            client: DaemonClient::new(socket),
            peer_id: PeerId([0x53; 32]),
            abi_version: arena0_client::protocol::ABI_VERSION,
            driver: DriverSpec::Builtin("sample".to_owned()),
        }];
        let (_cancel, mut cancelled) = watch::channel(None);
        let progress = test_progress();

        let error = progress
            .during(
                RunStage::Negotiation,
                hosts.len(),
                create_executions(
                    &hosts,
                    ProgramHash([0x54; 32]),
                    None,
                    &mut cancelled,
                    &progress,
                ),
            )
            .await
            .expect_err("mismatched id must fail creation");
        assert!(format!("{error:#}").contains(&returned_exec.to_string()));
        assert!(error.to_string().contains("was cleaned up"));

        let requests = server.await.expect("scripted Host");
        let HostRequest::ExecNew {
            exec_id: requested_exec,
            ..
        } = &requests[0]
        else {
            panic!("first request was not exec.new");
        };
        assert!(matches!(
            requests[1],
            HostRequest::ExecCancelCreation { exec_id } if exec_id == *requested_exec
        ));
    }

    #[tokio::test]
    async fn join_failure_withdraws_creator_and_successful_joiners() {
        let directory = tempfile::tempdir().expect("socket directory");
        let program_id = ProgramHash([0x11; 32]);
        let negotiation_id = arena0_client::protocol::NegotiationId([0x12; 32]);

        let mut hosts = Vec::new();
        let mut servers = Vec::new();
        for (index, name) in ["creator", "joined", "rejected"].into_iter().enumerate() {
            let socket = directory.path().join(format!("{name}.sock"));
            let listener = UnixListener::bind(&socket).expect("bind scripted Host");
            let responses = match index {
                0 => vec![
                    created_response(ExecId([0; 32]), negotiation_id),
                    Ok(ResponseOk::Ack),
                ],
                1 => vec![
                    created_response(ExecId([0; 32]), negotiation_id),
                    Ok(ResponseOk::Ack),
                ],
                _ => vec![
                    Err(ApiError::new(
                        ApiErrorCode::Negotiation,
                        "injected join rejection",
                    )),
                    Ok(ResponseOk::Ack),
                ],
            };
            servers.push(tokio::spawn(serve_script(listener, host(name), responses)));
            hosts.push(HostConnection {
                host: host(name),
                client: DaemonClient::new(socket),
                peer_id: PeerId([index as u8 + 1; 32]),
                abi_version: arena0_client::protocol::ABI_VERSION,
                driver: DriverSpec::Builtin("sample".to_owned()),
            });
        }

        let (_cancel, mut cancelled) = watch::channel(None);
        let progress = test_progress();
        let error = progress
            .during(
                RunStage::Negotiation,
                hosts.len(),
                create_executions(&hosts, program_id, None, &mut cancelled, &progress),
            )
            .await
            .expect_err("one rejected join must fail creation");
        assert!(error.to_string().contains("injected join rejection"));
        assert!(error.to_string().contains("withdrawn executions"));
        assert_eq!(
            progress.observations().last(),
            Some(&crate::progress::RunProgressState::Idle)
        );
        progress.terminal(RunTerminalState::Failed);
        assert_eq!(
            progress.observations().last(),
            Some(&crate::progress::RunProgressState::Terminal(
                RunTerminalState::Failed
            ))
        );

        let creator_requests = servers.remove(0).await.expect("creator script task");
        let joiner_requests = servers.remove(0).await.expect("joiner script task");
        let rejected_requests = servers.remove(0).await.expect("rejected script task");
        let HostRequest::ExecNew {
            exec_id: creator_exec,
            ..
        } = &creator_requests[0]
        else {
            panic!("creator did not receive exec.new");
        };
        let HostRequest::ExecNew {
            exec_id: joiner_exec,
            ..
        } = &joiner_requests[0]
        else {
            panic!("joiner did not receive exec.new");
        };
        assert!(matches!(
            creator_requests[1],
            HostRequest::ExecCancelCreation { exec_id } if exec_id == *creator_exec
        ));
        assert!(matches!(
            joiner_requests[1],
            HostRequest::ExecCancelCreation { exec_id } if exec_id == *joiner_exec
        ));
        let HostRequest::ExecNew {
            exec_id: rejected_exec,
            ..
        } = &rejected_requests[0]
        else {
            panic!("rejected Host did not receive exec.new");
        };
        assert!(matches!(
            rejected_requests[1],
            HostRequest::ExecCancelCreation { exec_id } if exec_id == *rejected_exec
        ));
    }

    #[tokio::test]
    async fn cancellation_during_join_withdraws_every_returned_execution() {
        let directory = tempfile::tempdir().expect("socket directory");
        let program_id = ProgramHash([0x31; 32]);
        let negotiation_id = arena0_client::protocol::NegotiationId([0x32; 32]);
        let creator_socket = directory.path().join("creator.sock");
        let joiner_socket = directory.path().join("joiner.sock");
        let creator_listener = UnixListener::bind(&creator_socket).expect("bind creator");
        let joiner_listener = UnixListener::bind(&joiner_socket).expect("bind joiner");
        let creator_server = tokio::spawn(serve_script(
            creator_listener,
            host("creator"),
            vec![
                created_response(ExecId([0; 32]), negotiation_id),
                Ok(ResponseOk::Ack),
            ],
        ));
        let (join_started, join_waiting) = oneshot::channel();
        let (release_join, join_released) = oneshot::channel();
        let joiner_server = tokio::spawn(async move {
            let (stream, _) = joiner_listener.accept().await.expect("accept join request");
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let request = read_host_request(&mut read, &host("joiner")).await;
            let HostRequest::ExecNew {
                exec_id: joiner_exec,
                ..
            } = &request
            else {
                panic!("joiner did not receive exec.new");
            };
            join_started.send(()).expect("report pending join");
            join_released.await.expect("release join response");
            arena0_client::api::frame::write_frame(
                &mut write,
                &created_response(*joiner_exec, negotiation_id),
            )
            .await
            .expect("write join response");
            let mut requests = vec![request];
            requests.extend(
                serve_script(joiner_listener, host("joiner"), vec![Ok(ResponseOk::Ack)]).await,
            );
            requests
        });
        let hosts = vec![
            HostConnection {
                host: host("creator"),
                client: DaemonClient::new(creator_socket),
                peer_id: PeerId([1; 32]),
                abi_version: arena0_client::protocol::ABI_VERSION,
                driver: DriverSpec::Builtin("sample".to_owned()),
            },
            HostConnection {
                host: host("joiner"),
                client: DaemonClient::new(joiner_socket),
                peer_id: PeerId([2; 32]),
                abi_version: arena0_client::protocol::ABI_VERSION,
                driver: DriverSpec::Builtin("sample".to_owned()),
            },
        ];
        let (cancel, mut cancelled) = watch::channel(None);
        let progress = test_progress();
        let task_progress = progress.clone();
        let creation = tokio::spawn(async move {
            task_progress
                .during(
                    RunStage::Negotiation,
                    hosts.len(),
                    create_executions(&hosts, program_id, None, &mut cancelled, &task_progress),
                )
                .await
        });

        join_waiting.await.expect("join became pending");
        cancel.send_replace(Some("injected cancellation".to_owned()));
        release_join.send(()).expect("release join response");
        let error = creation
            .await
            .expect("creation task")
            .expect_err("cancellation must fail creation");
        assert!(error.to_string().contains("injected cancellation"));
        assert!(error.to_string().contains("withdrawn executions"));
        assert_eq!(
            progress.observations().last(),
            Some(&crate::progress::RunProgressState::Idle)
        );
        progress.terminal(RunTerminalState::Cancelled);
        assert_eq!(
            progress.observations().last(),
            Some(&crate::progress::RunProgressState::Terminal(
                RunTerminalState::Cancelled
            ))
        );

        let creator_requests = creator_server.await.expect("creator server");
        let joiner_requests = joiner_server.await.expect("joiner server");
        let HostRequest::ExecNew {
            exec_id: creator_exec,
            ..
        } = &creator_requests[0]
        else {
            panic!("creator did not receive exec.new");
        };
        let HostRequest::ExecNew {
            exec_id: joiner_exec,
            ..
        } = &joiner_requests[0]
        else {
            panic!("joiner did not receive exec.new");
        };
        assert!(matches!(
            creator_requests[1],
            HostRequest::ExecCancelCreation { exec_id } if exec_id == *creator_exec
        ));
        assert!(matches!(
            joiner_requests[1],
            HostRequest::ExecCancelCreation { exec_id } if exec_id == *joiner_exec
        ));
    }

    #[test]
    fn binding_validation_rejects_duplicates_and_json_humans() {
        let duplicate = vec![
            DriverBinding::new(host("host-01"), DriverSpec::Builtin("first-allowed".into())),
            DriverBinding::new(host("host-01"), DriverSpec::Builtin("first-allowed".into())),
        ];
        assert!(
            ValidatedBindings::new(duplicate, HumanFrontend::None)
                .unwrap_err()
                .to_string()
                .contains("selected more than once")
        );

        let human = vec![
            DriverBinding::new(host("host-01"), DriverSpec::Human),
            DriverBinding::new(host("host-02"), DriverSpec::Builtin("first-allowed".into())),
        ];
        assert!(
            ValidatedBindings::new(human, HumanFrontend::None)
                .unwrap_err()
                .to_string()
                .contains("interactive run")
        );
    }

    #[test]
    fn shared_tui_accepts_multiple_human_hosts_but_inline_input_does_not() {
        let humans = || {
            vec![
                DriverBinding::new(host("host-01"), DriverSpec::Human),
                DriverBinding::new(host("host-02"), DriverSpec::Human),
            ]
        };

        assert!(ValidatedBindings::new(humans(), HumanFrontend::SharedTui).is_ok());
        assert!(
            ValidatedBindings::new(humans(), HumanFrontend::InlineSingle)
                .unwrap_err()
                .to_string()
                .contains("at most one human driver")
        );
    }

    #[test]
    fn first_allowed_policy_only_answers_closed_enums() {
        let answer = deterministic_builtin_answer(
            "first-allowed",
            "ChooseAction",
            &Value::Null,
            &schema(json!({"enum": ["Rock", "Paper", "Scissors"]})),
        )
        .expect("enum should be answerable");
        assert_eq!(answer, json!("Rock"));

        let error = deterministic_builtin_answer(
            "first-allowed",
            "OpenAnswer",
            &Value::Null,
            &json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "string"
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("allowed enum"));
    }

    #[test]
    fn sample_policy_covers_open_supported_program_callouts() {
        let move_answer = deterministic_builtin_answer(
            "sample",
            "MakeMove",
            &json!({"legal_moves": "a2a3, a2a4"}),
            &json!({"type": "string"}),
        )
        .expect("sample chess policy");
        assert_eq!(move_answer, json!("a2a3"));

        let bid_answer = deterministic_builtin_answer(
            "sample",
            "SubmitBid",
            &json!({"item": "lot"}),
            &json!({"type": "integer"}),
        )
        .expect("sample auction policy");
        assert_eq!(bid_answer, json!(0));

        let offer_answer = deterministic_builtin_answer(
            "sample",
            "SubmitOffer",
            &json!({
                "tasks": [
                    {"name": "one", "capability": "cpu"},
                    {"name": "two", "capability": "gpu"}
                ],
                "maximum_capacity": 2,
                "maximum_cost": 100,
            }),
            &json!({"type": "object"}),
        )
        .expect("sample contract-net policy");
        assert_eq!(
            offer_answer,
            json!({
                "capabilities": ["cpu", "gpu"],
                "capacity": 2,
                "bids": [
                    {"task": 0, "cost": 1},
                    {"task": 1, "cost": 2}
                ]
            })
        );
    }

    #[test]
    fn evidence_comparison_rejects_terminal_disagreement() {
        let peer_a = PeerId([1; 32]);
        let peer_b = PeerId([2; 32]);
        let session_id = SessionHash([3; 32]);
        let program_id = ProgramHash([4; 32]);
        let ensemble = vec![peer_a, peer_b];
        let result = VerifiedResult::Light {
            terminal: arena0_client::api::LightVerifiedTerminal::Completed {
                outcome_borsh: vec![1],
            },
        };
        let mut second = result.clone();
        if let VerifiedResult::Light {
            terminal: arena0_client::api::LightVerifiedTerminal::Completed { outcome_borsh },
        } = &mut second
        {
            outcome_borsh.push(2);
        }
        let receipts = vec![
            HostEvidence {
                receipt_id: arena0_client::protocol::ReceiptId::from_bytes([9; 32]),
                peer_id: peer_a,
                program_id,
                session_id,
                ensemble: ensemble.clone(),
                steps: 1,
                result,
            },
            HostEvidence {
                receipt_id: arena0_client::protocol::ReceiptId::from_bytes([9; 32]),
                peer_id: peer_b,
                program_id,
                session_id,
                ensemble,
                steps: 1,
                result: second,
            },
        ];
        assert!(
            compare_evidence(&receipts)
                .unwrap_err()
                .to_string()
                .contains("terminal result")
        );
    }

    #[test]
    fn matching_outcomes_do_not_hide_different_receipt_ids() {
        let first = HostEvidence {
            peer_id: PeerId([1; 32]),
            receipt_id: arena0_client::protocol::ReceiptId::from_bytes([3; 32]),
            program_id: ProgramHash([4; 32]),
            session_id: SessionHash([5; 32]),
            ensemble: vec![PeerId([1; 32]), PeerId([2; 32])],
            steps: 1,
            result: VerifiedResult::Light {
                terminal: LightVerifiedTerminal::Completed {
                    outcome_borsh: vec![7],
                },
            },
        };
        let mut second = first.clone();
        second.peer_id = PeerId([2; 32]);
        assert!(compare_evidence(&[first.clone(), second.clone()]).is_ok());
        second.receipt_id = arena0_client::protocol::ReceiptId::from_bytes([6; 32]);
        assert!(
            compare_evidence(&[first, second])
                .unwrap_err()
                .to_string()
                .contains("different receipt artifacts")
        );
    }

    #[test]
    fn full_replay_outcome_must_match_the_live_terminal() {
        let verified = VerifiedResult::Full {
            terminal: arena0_client::api::FullVerifiedTerminal::Completed {
                outcome_borsh: vec![1],
                outcome_json: json!({"winner": 1}),
            },
        };
        let live = || TerminalConsensus::Completed {
            session_id: SessionHash([7; 32]),
            outcome: Some(json!({"winner": 1})),
        };
        assert_eq!(
            bind_verified_terminal(live(), &verified).expect("matching replay outcome"),
            AggregateTerminal::Completed {
                outcome: Some(json!({"winner": 1}))
            }
        );
        assert!(
            bind_verified_terminal(
                TerminalConsensus::Completed {
                    session_id: SessionHash([7; 32]),
                    outcome: Some(json!({"winner": 2})),
                },
                &verified,
            )
            .unwrap_err()
            .to_string()
            .contains("disagrees")
        );
    }

    #[test]
    fn authenticated_stop_is_classified_only_from_verified_evidence() {
        use arena0_client::protocol::{CHAIN_START, STEP_COMMIT_DOMAIN, StateHash, StepCommitment};

        let stopped = VerifiedResult::Light {
            terminal: arena0_client::api::LightVerifiedTerminal::Stopped {
                cause: StopCause::Shared {
                    kind: AbortKind::Abort,
                    commitment: StepCommitment {
                        domain: STEP_COMMIT_DOMAIN,
                        session_id: SessionHash([8; 32]),
                        step: 2,
                        entry_hash: [9; 32],
                        pre_state: StateHash([10; 32]),
                        post_state: StateHash([11; 32]),
                        link: CHAIN_START,
                    },
                    reason: "operator stopped".to_owned(),
                },
            },
        };
        assert_eq!(
            bind_verified_terminal(TerminalConsensus::Stopped, &stopped).expect("verified stop"),
            AggregateTerminal::Stopped
        );
        assert_eq!(
            AggregateTerminal::Stopped.progress_state(),
            RunTerminalState::Failed
        );
        assert_eq!(
            AggregateTerminal::Failed.progress_state(),
            RunTerminalState::Failed
        );
        assert_eq!(
            AggregateTerminal::Completed { outcome: None }.progress_state(),
            RunTerminalState::Succeeded
        );
        assert!(
            bind_verified_terminal(
                TerminalConsensus::Completed {
                    session_id: SessionHash([8; 32]),
                    outcome: None,
                },
                &stopped,
            )
            .unwrap_err()
            .to_string()
            .contains("disagrees")
        );
    }
}
