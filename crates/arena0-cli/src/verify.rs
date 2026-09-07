//! `arena0 verify`: spelled-out receipt evidence. Three target kinds:
//!
//! - a **session id** or **receipt id** the daemon holds -> the daemon verifies and
//!   returns the evidence;
//! - a **file path** -> verified daemonlessly against `arena0-verify` (light needs no
//!   daemon at all; `--replay` delegates replay to a running Host).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, bail};
use arena0_client::api::{
    FullVerifiedTerminal as ApiFullVerifiedTerminal, HostRequest,
    LightVerifiedTerminal as ApiLightVerifiedTerminal, ReceiptArtifact, ReceiptRef, ResponseOk,
    VerifiedResult,
};
use arena0_client::proto::DaemonClient;
use arena0_client::protocol::{ABI_VERSION, PeerId, ProgramHash, SessionHash};
use arena0_home::HostName;
use arena0_verify::{LightVerifiedTerminal as LightVerifiedTerminalBytes, verify_light};
use serde_json::json;
use tokio::task::JoinSet;

use crate::coordinated::{EvidenceAgreement, HostEvidence, compare_evidence};
use crate::{Ctx, ui};

/// The evidence a verification recovers, rendered the same whether it came from the
/// daemon or a daemonless run.
struct Evidence {
    receipt_id: arena0_client::protocol::ReceiptId,
    program_id: ProgramHash,
    program_name: Option<String>,
    session_id: SessionHash,
    ensemble: Vec<PeerId>,
    steps: u64,
    result: VerifiedResult,
    local_peer: Option<PeerId>,
}

pub(crate) async fn verify(ctx: &Ctx, target: String, full: bool) -> anyhow::Result<()> {
    let path = Path::new(&target);
    if is_path_target(path, &target) {
        verify_file(ctx, path, full).await
    } else {
        verify_resident(ctx, &target, full).await
    }
}

/// Verify every Host of one resident session through independently named
/// local Hosts, then require their authenticated shared evidence to agree.
pub(crate) async fn verify_hosts(
    mode: ui::Mode,
    palette: ui::Palette,
    target: &str,
    hosts: &[HostName],
    replay: bool,
) -> anyhow::Result<()> {
    let mut names = HashSet::with_capacity(hosts.len());
    for host in hosts {
        if !names.insert(host.clone()) {
            bail!("Host '{host}' was selected more than once");
        }
    }

    let mut jobs = JoinSet::new();
    for (index, host) in hosts.iter().cloned().enumerate() {
        let target = target.to_owned();
        jobs.spawn(async move {
            let client = DaemonClient::from_env()
                .with_context(|| format!("resolve daemon endpoint for Host '{host}'"))?;
            let info = match client.call_host(&host, &HostRequest::Info).await? {
                ResponseOk::HostStatus(info) => info,
                other => bail!("unexpected daemon.info response from Host '{host}': {other:?}"),
            };
            let key = client
                .resolve_produced_receipt_ref(&host, &target)
                .await
                .with_context(|| format!("resolve receipt on Host '{host}'"))?;
            let response = client
                .call_host(
                    &host,
                    &HostRequest::ReceiptVerify {
                        receipt: key,
                        full: replay,
                    },
                )
                .await
                .with_context(|| format!("verify Host receipt on Host '{host}'"))?;
            let ResponseOk::Verified {
                receipt_id,
                program_id,
                session_id,
                ensemble,
                steps,
                result,
            } = response
            else {
                bail!("unexpected receipt.verify response from Host '{host}'");
            };
            Ok::<_, anyhow::Error>((
                index,
                host,
                HostEvidence {
                    receipt_id,
                    peer_id: info.host.peer_id,
                    program_id,
                    session_id,
                    ensemble,
                    steps,
                    result,
                },
            ))
        });
    }

    let mut ordered = (0..hosts.len())
        .map(|_| None)
        .collect::<Vec<Option<(HostName, HostEvidence)>>>();
    while let Some(joined) = jobs.join_next().await {
        let (index, host, evidence) =
            joined.context("Host receipt verification task failed to join")??;
        ordered[index] = Some((host, evidence));
    }
    let evidence = ordered
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            entry.ok_or_else(|| anyhow::anyhow!("verification result for Host {index} missing"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let receipts = evidence
        .iter()
        .map(|(_, receipt)| receipt.clone())
        .collect::<Vec<_>>();
    let agreement = compare_evidence(&receipts)?;
    let producers = receipts
        .iter()
        .map(|receipt| receipt.peer_id)
        .collect::<HashSet<_>>();
    if producers.len() != hosts.len()
        || agreement.ensemble.len() != hosts.len()
        || !agreement
            .ensemble
            .iter()
            .all(|peer| producers.contains(peer))
    {
        bail!(
            "--hosts must name every independent peer_id in the {}-party receipt ensemble",
            agreement.ensemble.len()
        );
    }

    render_host_evidence(mode, palette, replay, &evidence, &agreement);
    Ok(())
}

/// Receipt IDs and session ids are bare opaque values. Any target with path
/// syntax is an explicit file target, even when it does not exist yet; falling
/// through to resident-id lookup would hide the actionable filesystem error.
pub(crate) fn is_path_target(path: &Path, target: &str) -> bool {
    path.is_file()
        || path.is_absolute()
        || target.contains('/')
        || target.contains('\\')
        || path.extension().is_some()
}

/// Verify a session/receipt id the daemon holds.
async fn verify_resident(ctx: &Ctx, target: &str, full: bool) -> anyhow::Result<()> {
    if !ctx.client().daemon_up().await {
        bail!(
            "daemon not reachable at {}. To verify a receipt file offline, pass its path.",
            ctx.client.socket().display()
        );
    }
    // A receipt-id prefix resolves to its exact peer_id; a session id uses the
    // addressed daemon's peer_id.
    let key = ctx.client().resolve_receipt_ref(&ctx.host, target).await?;
    let resp = ctx
        .call(&HostRequest::ReceiptVerify { receipt: key, full })
        .await?;
    let ResponseOk::Verified {
        receipt_id,
        program_id,
        session_id,
        ensemble,
        steps,
        result,
    } = resp
    else {
        bail!("unexpected verify response");
    };
    let names = ctx.client().program_name_map(&ctx.host).await;
    let local_peer = node_peer_id(ctx).await;
    render(
        ctx.mode,
        ctx.palette,
        &Evidence {
            receipt_id,
            program_name: names.get(&program_id).cloned(),
            program_id,
            session_id,
            ensemble,
            steps,
            result,
            local_peer,
        },
    );
    Ok(())
}

/// Verify a receipt file offline. Light needs no daemon; full belongs to a Host.
async fn verify_file(ctx: &Ctx, path: &Path, full: bool) -> anyhow::Result<()> {
    let receipt = full.then(|| read_receipt(path)).transpose()?;

    // Full replay belongs to the Host, which owns the registered program bytes and
    // replay sandbox. The public client never links the sandbox itself.
    if full {
        if ctx.client().daemon_up().await {
            return delegate_to_daemon(ctx, receipt.expect("full receipt preflight"), true).await;
        }
        return full_replay_requires_daemon();
    }

    verify_file_light(Some(ctx), ctx.mode, ctx.palette, path).await
}

/// Verify a receipt file without constructing a daemon client. Optional daemon
/// enrichment is deliberately absent when no explicit socket was supplied.
pub(crate) async fn verify_offline(
    mode: crate::ui::Mode,
    palette: crate::ui::Palette,
    path: &Path,
) -> anyhow::Result<()> {
    verify_file_light(None, mode, palette, path).await
}

/// Explain why full replay cannot proceed without a resolvable daemon socket.
pub(crate) fn full_replay_requires_daemon() -> anyhow::Result<()> {
    bail!(
        "full replay requires a running arena0d Host; use light verification \
         for an offline receipt"
    );
}

/// Read and parse an explicit receipt file before consulting a Host.
pub(crate) fn read_receipt(path: &Path) -> anyhow::Result<ReceiptArtifact> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse receipt {}", path.display()))
}

/// Verify a receipt file's portable proof and render its evidence.
async fn verify_file_light(
    ctx: Option<&Ctx>,
    mode: crate::ui::Mode,
    palette: crate::ui::Palette,
    path: &Path,
) -> anyhow::Result<()> {
    let receipt = read_receipt(path)?;
    let receipt_id = receipt.receipt_id();

    // Light: fully offline.
    let encoded = receipt
        .encode()
        .map_err(|error| anyhow::anyhow!("encode receipt {}: {error}", path.display()))?;
    let light = verify_light(&encoded).map_err(|e| anyhow::anyhow!("FAILED: {e}"))?;
    let result = VerifiedResult::Light {
        terminal: terminal_of(&light.terminal),
    };
    let (program_name, local_peer) = match ctx {
        Some(ctx) => enrich_offline(ctx, light.program_id).await,
        None => (None, None),
    };
    render(
        mode,
        palette,
        &Evidence {
            receipt_id,
            program_name,
            program_id: light.program_id,
            session_id: light.session_id,
            ensemble: light.ensemble,
            steps: light.steps,
            result,
            local_peer,
        },
    );
    Ok(())
}

/// Delegate a file's verification to a reachable daemon (it holds the wasm).
async fn delegate_to_daemon(ctx: &Ctx, receipt: ReceiptArtifact, full: bool) -> anyhow::Result<()> {
    let resp = ctx
        .call(&HostRequest::ReceiptVerify {
            receipt: ReceiptRef::Inline(Box::new(receipt)),
            full,
        })
        .await?;
    let ResponseOk::Verified {
        receipt_id,
        program_id,
        session_id,
        ensemble,
        steps,
        result,
    } = resp
    else {
        bail!("unexpected verify response");
    };
    let names = ctx.client().program_name_map(&ctx.host).await;
    let local_peer = node_peer_id(ctx).await;
    render(
        ctx.mode,
        ctx.palette,
        &Evidence {
            receipt_id,
            program_name: names.get(&program_id).cloned(),
            program_id,
            session_id,
            ensemble,
            steps,
            result,
            local_peer,
        },
    );
    Ok(())
}

/// Best-effort enrichment for offline evidence: the program's handle name and our
/// own peer id, all only if a daemon happens to be reachable.
async fn enrich_offline(ctx: &Ctx, program_id: ProgramHash) -> (Option<String>, Option<PeerId>) {
    if !ctx.client().daemon_up().await {
        return (None, None);
    }
    let local_peer = node_peer_id(ctx).await;
    let mut name = None;
    if let Ok(ResponseOk::Program(detail)) = ctx
        .call(&HostRequest::ProgramGet {
            program: program_id.to_string(),
        })
        .await
    {
        name = Some(detail.summary.name.clone());
    }
    (name, local_peer)
}

async fn node_peer_id(ctx: &Ctx) -> Option<PeerId> {
    match ctx.call(&HostRequest::Info).await {
        Ok(ResponseOk::HostStatus(i)) => Some(i.host.peer_id),
        _ => None,
    }
}

/// Convert light verifier evidence into the API's light-tier result.
fn terminal_of(t: &LightVerifiedTerminalBytes) -> ApiLightVerifiedTerminal {
    match t {
        LightVerifiedTerminalBytes::Completed { outcome_borsh } => {
            ApiLightVerifiedTerminal::Completed {
                outcome_borsh: outcome_borsh.clone(),
            }
        }
        LightVerifiedTerminalBytes::Stopped { cause } => ApiLightVerifiedTerminal::Stopped {
            cause: cause.clone(),
        },
    }
}

fn render(mode: crate::ui::Mode, palette: crate::ui::Palette, ev: &Evidence) {
    if mode.is_json() {
        crate::ui::print_json(&json!({
            "tier": result_tier(&ev.result),
            "program_id": ev.program_id.to_string(),
            "program": ev.program_name,
            "session_id": ev.session_id.to_string(),
            "receipt_id": ev.receipt_id.to_string(),
            "ensemble": ev.ensemble.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "steps": ev.steps,
            "result": ev.result,
        }));
        return;
    }

    let p = palette;
    let tier = result_tier(&ev.result);
    println!("{} ({tier})", p.green("verified"));
    let name = ev
        .program_name
        .clone()
        .unwrap_or_else(|| "(unregistered)".into());
    println!(
        "  program     {}  ({}, abi {})",
        name,
        ev.program_id.fmt_short(),
        ABI_VERSION
    );
    println!("  session     {}", ev.session_id.fmt_short());
    println!("  receipt     {}", ev.receipt_id);
    println!(
        "  ensemble    {}",
        render_ensemble(&ev.ensemble, ev.local_peer)
    );
    match &ev.result {
        VerifiedResult::Light {
            terminal: ApiLightVerifiedTerminal::Completed { .. },
        } => {
            println!("  steps       {}, chain intact, all agreed", ev.steps);
            println!("  outcome     (JSON projection unavailable in light verification)");
        }
        VerifiedResult::Full {
            terminal: ApiFullVerifiedTerminal::Completed { outcome_json, .. },
        } => {
            println!("  steps       {}, chain intact, all agreed", ev.steps);
            println!("  outcome     {}", crate::ui::compact_json(outcome_json));
        }
        VerifiedResult::Light {
            terminal: ApiLightVerifiedTerminal::Stopped { cause },
        }
        | VerifiedResult::Full {
            terminal: ApiFullVerifiedTerminal::Stopped { cause },
        } => {
            println!("  steps       {}, chain intact", ev.steps);
            println!("  {}     stopped: {cause:?}", p.yellow("terminal"));
        }
    }
}

fn render_host_evidence(
    mode: ui::Mode,
    palette: ui::Palette,
    replay: bool,
    evidence: &[(HostName, HostEvidence)],
    agreement: &EvidenceAgreement,
) {
    let tier = if replay { "full" } else { "light" };
    if mode.is_json() {
        ui::print_json(&json!({
            "tier": tier,
            "program_id": agreement.program_id.to_string(),
            "session_id": agreement.session_id.to_string(),
            "receipt_id": agreement.receipt_id.to_string(),
            "ensemble": agreement.ensemble.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "steps": agreement.steps,
            "result": agreement.result,
            "producers": evidence.iter().map(|(host, receipt)| json!({
                "host": host.to_string(),
                "peer_id": receipt.peer_id.to_string(),
                "result": "valid",
            })).collect::<Vec<_>>(),
            "all_verified": true,
            "shared_evidence_agrees": true,
        }));
        return;
    }

    println!(
        "{} {}/{} Host receipts ({})",
        palette.green("verified"),
        evidence.len(),
        agreement.ensemble.len(),
        if replay { "full replay" } else { "light" }
    );
    println!("  program     {}", agreement.program_id.fmt_short());
    println!("  session     {}", agreement.session_id.fmt_short());
    println!("  receipt     {}", agreement.receipt_id);
    println!("  steps       {}", agreement.steps);
    for (host, receipt) in evidence {
        println!("  peer_id    {}  {}", host, receipt.peer_id.fmt_short());
    }
    match &agreement.result {
        VerifiedResult::Full {
            terminal: ApiFullVerifiedTerminal::Completed { outcome_json, .. },
        } => println!("  outcome     {}", ui::compact_json(outcome_json)),
        VerifiedResult::Light {
            terminal: ApiLightVerifiedTerminal::Completed { .. },
        } => println!("  outcome     (JSON projection unavailable in light verification)"),
        VerifiedResult::Light {
            terminal: ApiLightVerifiedTerminal::Stopped { cause },
        }
        | VerifiedResult::Full {
            terminal: ApiFullVerifiedTerminal::Stopped { cause },
        } => println!("  {}     stopped: {cause:?}", palette.yellow("terminal")),
    }
}

fn result_tier(result: &VerifiedResult) -> &'static str {
    match result {
        VerifiedResult::Light { .. } => "light",
        VerifiedResult::Full { .. } => "full replay",
    }
}

/// Render the ensemble as `peer (idx)`, marking our own identity as `you`.
fn render_ensemble(ensemble: &[PeerId], local: Option<PeerId>) -> String {
    ensemble
        .iter()
        .enumerate()
        .map(|(i, peer)| {
            if Some(*peer) == local {
                format!("you ({i})")
            } else {
                format!("{} ({i})", peer.fmt_short())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_shaped_targets_are_never_resident_references() {
        for target in [
            "receipt.json",
            "./receipt.json",
            "../receipt.json",
            "/tmp/receipt.json",
            "subdir/receipt",
            r"C:\\receipts\\receipt.json",
        ] {
            assert!(
                is_path_target(Path::new(target), target),
                "expected path target: {target}"
            );
        }
    }

    #[test]
    fn existing_extensionless_files_are_path_targets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("receipt");
        std::fs::write(&path, b"not a receipt").expect("write fixture");

        assert!(is_path_target(&path, path.to_str().expect("UTF-8 path")));
    }

    #[test]
    fn any_filename_extension_is_a_path_target() {
        for target in ["receipt.bin", "receipt.receipt", "receipt.txt"] {
            assert!(is_path_target(Path::new(target), target));
        }
    }

    #[test]
    fn bare_ids_remain_resident_references() {
        for target in [
            "a".repeat(64),
            "receipt-id".to_owned(),
            "session-id".to_owned(),
        ] {
            assert!(!is_path_target(Path::new(&target), &target));
        }
    }
}
