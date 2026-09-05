//! Full-screen execution observatory for one coordinated run.
//!
//! # Target screen hierarchy
//!
//! This module presents the same run at two levels. The Overview answers what
//! is happening now. Each detail view answers a different diagnostic question
//! and exposes the records that support its answer.
//!
//! ```text
//! masthead: program identity and run state
//! numeric workspace tabs
//! body
//! ├── Hosts: persistent All, one-Host, and comparison navigator
//! └── evidence canvas
//!     ├── Overview mosaic
//!     │   ├── Program summary
//!     │   ├── Negotiation summary
//!     │   ├── Public Trace summary
//!     │   ├── WASM summary
//!     │   └── System Events summary
//!     ├── Negotiation detail
//!     ├── Program detail
//!     ├── Public Trace detail
//!     ├── WASM detail
//!     ├── System Events detail
//!     └── pending callout dock
//! transient help, filter, and record inspectors
//! ```
//!
//! The tab bar remains visible in every workspace view. The Overview keeps all
//! five summaries visible in a responsive collapsed-border mosaic. Number keys `1` through
//! `6` are the only workspace-tab navigation. `Tab` and `Shift-Tab` move focus
//! only among controls inside the current view: Hosts, records, inspector, and
//! composer. On the Overview they traverse Hosts and the five summary panes.
//! `Enter` activates the cursor in Hosts or opens the focused summary, and `Escape` returns from a detail
//! view to the Overview. Arrow keys, page keys, `Home`, and `End` operate on
//! that view's records. `Enter` inspects the selected record. `Escape` closes
//! an inspector before it returns to the Overview.
//!
//! The Host sidebar is the only owner of Host navigation; detail panes must not
//! add a second Host picker. Human control and Host scope are independent. Control says which Host
//! callouts the operator answers. Scope says whether every pane projects all
//! Hosts, one Host, or a two-Host comparison. Changing scope never transfers
//! control. The sidebar remains visible in every workspace view and marks the
//! operator-controlled Host, pending input, and comparison sides directly on
//! their Host rows.
//!
//! A pending callout's complete input box remains visible in every workspace
//! view while the user inspects the run. At 88 columns and above it docks under
//! the evidence canvas while Hosts remain visible; below 88 columns it spans
//! the terminal to protect prompt width. Each queued Host request owns its own
//! [`TextArea`], prompt, context, schema, and validation error. Other views may
//! report that input is pending, but they must not duplicate the composer. Its
//! height follows its semantic rows: prompt, request identity, options when
//! present, context when present, validation error when present, and editor.
//! Those rows wrap to remain readable, and multiline drafts retain their own
//! cursor and selection state when the operator switches pending requests.
//! The workspace yields every row the composer needs before its own summaries
//! compete for space. If the terminal cannot contain that request plus the
//! persistent chrome, an explicit input-only mode pins the editor and lets
//! `Ctrl-PageUp` and `Ctrl-PageDown` scroll request metadata.
//!
//! On a wide Overview, Program and Negotiation share the first row, Public
//! Trace and WASM share the second, and System Events consumes the remainder.
//! Program receives its natural rendered height when possible, so a complete
//! board is never traded for blank dashboard space. Medium and narrow layouts
//! recompose the same five summaries instead of shrinking the wide grid.
//!
//! Color uses semantic ANSI roles over the terminal's default background.
//! `ARENA0_THEME=dark` and `ARENA0_THEME=light` select contrast-safe variants;
//! without an override, `COLORFGBG` supplies a best-effort light-background
//! hint. `NO_COLOR` keeps the same focus and status distinctions in attributes
//! and text.
//!
//! ## Overview and detail views have different jobs
//!
//! Do not implement an Overview pane by rendering its detail view into a
//! smaller rectangle. Derive separate summary and detail projections from the
//! same typed state:
//!
//! | View | Overview answer | Detail answer |
//! | --- | --- | --- |
//! | Negotiation | Which stage is active, and have the scoped Hosts converged? | Which Host or participant is waiting, and what observed or durable evidence does it hold? |
//! | Program | What does each scoped Host show now? | What did each Host's program-owned view show at each retained step? |
//! | Public Trace | Which public transitions have the scoped Hosts observed? | Which Host observed each event, state edge, fuel charge, and agreement at a selected public step? |
//! | WASM | Which shared and Host-private handlers have run? | Which public handler established each boundary, which private handlers ran afterward on each Host, and what effects did they return? |
//! | System Events | What did the Hosts most recently report? | What did each Host observe in its own sequence, and what safe fields belong to the selected event? |
//!
//! Detail views use the widget that matches their data. Use a stateful table
//! for comparable records, indented parent-child rows for a causal hierarchy,
//! and a paragraph for program-owned text or a selected record's fields. Use a gauge
//! only for a real ratio such as accepted tickets or verified receipts. Keep
//! selection state across redraws and identify rows with domain keys rather
//! than vector positions. On wide terminals, place the record collection and
//! its inspector side by side. On narrow terminals, keep the records readable
//! and open the inspector as a modal instead of compressing both regions.
//!
//! The five detail views are intentionally not one generic table screen:
//!
//! - Negotiation is a stage summary, a Host matrix, and evidence for the
//!   selected Host. Observed negotiation events are Host-local runtime facts.
//!   An activation record is durable evidence. Keep that distinction visible.
//! - Program is a Host and step navigator beside the complete guest-owned
//!   [`View`]. Identify the complete view once as Wasm program output, then use
//!   the plain `Header`, `Agents`, `State`, and `StatusBar` slot names. Do not
//!   interpret their contents as protocol state.
//! - Public Trace is a table keyed by public step. Its inspector owns the
//!   event, pre-state and post-state hashes, bounded message projection, fuel,
//!   agreement, witness, and public effects.
//! - WASM preserves the execution hierarchy. A private commit at public cursor
//!   `N` ran after the public entries `0..N`, so render it beneath public step
//!   `N - 1`; cursor zero is before the first public step. Private sequence is
//!   Host-local and must not imply a global order between Hosts. Never invent
//!   a public entry merely because a private cursor refers to its boundary.
//!   Private summaries remain bounded pages; `<` and `>` load the selected
//!   Host's adjacent page without exposing private payloads.
//! - System Events is a bounded, redacted event table keyed by Host boot and
//!   local sequence. Wall-clock display order is not a global causal order,
//!   and this stream is not a durable audit log.
//!
//! ## State and trust boundaries
//!
//! [`RunUpdate`] is the TUI input boundary. [`ScreenState`] owns the latest
//! typed observations and the navigation state. Renderers derive terminal
//! projections from that state and perform no I/O. Pane-specific interaction
//! state belongs with its pane; do not reuse one scroll offset or row cursor
//! across unrelated views.
//!
//! Preserve the protocol's visibility rules in every projection and
//! inspector. Private records expose event kinds, sizes, effects, and fuel,
//! never payloads. Program Borsh values remain opaque. Public message decoding
//! is a bounded, best-effort diagnostic projection. It cannot affect execution
//! or proof semantics. System events remain redacted. If an inspection response
//! contains fewer private records than its total, show `visible X of Y`; do
//! not render invented placeholder records.
//!
//! Shared helpers may own collapsed borders, table styling, scrollbars,
//! responsive breakpoints, empty states, and inspector chrome. Each detail
//! view should own its typed projection, semantic selection key, input
//! handling, responsive composition, and renderer. Do not introduce a generic
//! pane trait that erases these domain differences.
//!
//! This hierarchy is the maintained architecture. Changes to a pane must
//! preserve its distinct question, semantic selection key, detail projection,
//! and route back to the Overview.

mod callouts;
use callouts::{CalloutQueue, PendingCallout};

mod navigation;
use navigation::{DetailRegion, Focus, OverviewPane, Page, WorkspaceView};

mod events;
mod negotiation;
mod overview;
mod program;
use overview::{overview_layout, render_overview};
#[cfg(test)]
use overview::{overview_program_content, overview_program_height};
mod trace;
mod wasm;

use crate::ui::TuiPalette;
use anyhow::{Context, anyhow};
use arena0_client::answer;
#[cfg(test)]
use arena0_client::api::PrivateCommitSummary;
use arena0_client::api::{
    ActivationInspection, EventData, EventFrame, ExecStatus, ExecutionInspection,
    PrivateEffectKind, PrivateEventKind, SessionTerminal,
};
use arena0_client::program::BorshSchemaDocument;
#[cfg(test)]
use arena0_client::protocol::TicketHash;
use arena0_client::protocol::{
    ExecId, ExecLifecycle, PeerId, PendingId, PublicEffect, PublicEvent, SessionHash, Slot,
    TraceEntry, View,
};
use arena0_client::sanitize;
use arena0_home::HostName;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt as _;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect, Spacing};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::merge::MergeStrategy;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui_textarea::TextArea;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const UPDATE_CAPACITY: usize = 32;
const MIN_WIDTH: u16 = 48;
const MIN_HEIGHT: u16 = 23;
const MAX_SYSTEM_EVENTS: usize = 256;
const MAX_VIEW_HISTORY: usize = 256;
const MIN_OVERVIEW_HEIGHT: u16 = 11;
pub(crate) const PRIVATE_INSPECTION_LIMIT: u16 = 256;

#[derive(Debug, Clone)]
pub(crate) struct TuiConfig {
    pub(crate) program: String,
    pub(crate) hosts: Vec<TuiHost>,
    pub(crate) message_schema: Option<BorshSchemaDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TuiHost {
    pub(crate) host: HostName,
    pub(crate) peer_id: PeerId,
    pub(crate) driver: TuiDriver,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TuiDriver {
    Human,
    Builtin(String),
    Agent,
}

impl TuiConfig {
    fn host_count(&self) -> usize {
        self.hosts.len()
    }

    fn first_host(&self) -> Option<&TuiHost> {
        self.hosts.first()
    }

    fn human_hosts(&self) -> impl Iterator<Item = &TuiHost> {
        self.hosts
            .iter()
            .filter(|host| host.driver == TuiDriver::Human)
    }
}

#[derive(Debug)]
pub(crate) enum RunUpdate {
    Status {
        host: HostName,
        status: ExecStatus,
    },
    Inspection {
        host: HostName,
        inspection: Box<ExecutionInspection>,
    },
    Agreement {
        host: HostName,
        agreed: u16,
        total: u16,
    },
    View {
        host: HostName,
        step: u64,
        view: View,
    },
    Trace {
        host: HostName,
        entries: Vec<TraceEntry>,
    },
    SystemEvent {
        frame: EventFrame,
    },
    Callout {
        host: HostName,
        exec_id: ExecId,
        pending_id: PendingId,
        callout_index: u32,
        name: String,
        prompt: String,
        context: Value,
        schema: Value,
        reply: oneshot::Sender<Value>,
    },
    ReceiptVerified {
        host: HostName,
        peer_id: PeerId,
        tier: &'static str,
    },
    VerificationProgress {
        verified: usize,
        total: usize,
        tier: &'static str,
    },
    Completed {
        outcome: Option<Value>,
    },
    Stopped {
        summary: String,
    },
    Failed {
        summary: String,
    },
    Close,
}

#[derive(Clone, Debug)]
pub(crate) struct TuiHandle {
    updates: mpsc::Sender<RunUpdate>,
    width: watch::Receiver<u16>,
    private_page: watch::Receiver<Option<PrivatePageRequest>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrivatePageRequest {
    pub(crate) host: HostName,
    pub(crate) from: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct TuiCalloutRequest {
    pub(crate) host: HostName,
    pub(crate) exec_id: ExecId,
    pub(crate) pending_id: PendingId,
    pub(crate) callout_index: u32,
    pub(crate) name: String,
    pub(crate) prompt: String,
    pub(crate) context: Value,
    pub(crate) schema: Value,
}

impl TuiHandle {
    #[cfg(test)]
    pub(crate) fn test_channel() -> (
        Self,
        mpsc::Receiver<RunUpdate>,
        watch::Sender<Option<PrivatePageRequest>>,
    ) {
        let (updates, receiver) = mpsc::channel(32);
        let (_, width) = watch::channel(80);
        let (pages, private_page) = watch::channel(None);
        (
            Self {
                updates,
                width,
                private_page,
            },
            receiver,
            pages,
        )
    }

    pub(crate) async fn update(&self, update: RunUpdate) -> anyhow::Result<()> {
        self.updates
            .send(update)
            .await
            .map_err(|_| anyhow!("run TUI closed"))
    }

    pub(crate) async fn answer(&self, request: TuiCalloutRequest) -> anyhow::Result<Value> {
        let (reply, answer) = oneshot::channel();
        self.update(RunUpdate::Callout {
            host: request.host,
            exec_id: request.exec_id,
            pending_id: request.pending_id,
            callout_index: request.callout_index,
            name: request.name,
            prompt: request.prompt,
            context: request.context,
            schema: request.schema,
            reply,
        })
        .await?;
        answer.await.context("run TUI closed before answering")
    }

    #[must_use]
    pub(crate) fn view_width(&self) -> u16 {
        *self.width.borrow()
    }

    pub(crate) async fn changed_width(&mut self) {
        if self.width.changed().await.is_err() {
            std::future::pending().await
        }
    }

    pub(crate) async fn changed_private_page(&mut self) -> Option<PrivatePageRequest> {
        if self.private_page.changed().await.is_err() {
            std::future::pending().await
        }
        self.private_page.borrow().clone()
    }
}

#[derive(Debug)]
pub(crate) struct TuiSession {
    handle: TuiHandle,
    task: Option<JoinHandle<anyhow::Result<UiExit>>>,
}

impl TuiSession {
    pub(crate) fn start(config: TuiConfig, cancel: watch::Sender<Option<String>>) -> Self {
        let (updates, receiver) = mpsc::channel(UPDATE_CAPACITY);
        let (width, width_rx) = watch::channel(80);
        let (private_page, private_page_rx) = watch::channel(None);
        let handle = TuiHandle {
            updates,
            width: width_rx,
            private_page: private_page_rx,
        };
        let task = tokio::spawn(run_screen(config, receiver, width, private_page, cancel));
        Self {
            handle,
            task: Some(task),
        }
    }

    #[must_use]
    pub(crate) fn handle(&self) -> TuiHandle {
        self.handle.clone()
    }

    pub(crate) async fn complete(&mut self, outcome: Option<Value>) -> anyhow::Result<()> {
        self.handle.update(RunUpdate::Completed { outcome }).await?;
        self.join().await.map(|_| ())
    }

    pub(crate) async fn close(&mut self) -> anyhow::Result<()> {
        let _ = self.handle.update(RunUpdate::Close).await;
        self.join().await.map(|_| ())
    }

    pub(crate) async fn fail(&mut self, summary: String) -> anyhow::Result<()> {
        self.handle.update(RunUpdate::Failed { summary }).await?;
        self.join().await.map(|_| ())
    }

    pub(crate) async fn stop(&mut self, summary: String) -> anyhow::Result<()> {
        self.handle.update(RunUpdate::Stopped { summary }).await?;
        self.join().await.map(|_| ())
    }

    async fn join(&mut self) -> anyhow::Result<UiExit> {
        let Some(task) = self.task.take() else {
            return Ok(UiExit::Closed);
        };
        task.await.context("run TUI task failed to join")?
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiExit {
    Completed,
    Cancelled,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostScope {
    All,
    One(HostName),
    Compare { left: HostName, right: HostName },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CrossingKey {
    Public { step: u64 },
    Boundary { after_position: u64 },
    Private { host: HostName, sequence: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventKey {
    host: String,
    boot_id: String,
    seq: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FollowState {
    offset: usize,
    new_count: usize,
}

impl FollowState {
    fn items_added(&mut self, count: usize) {
        if self.offset != 0 {
            self.new_count = self.new_count.saturating_add(count);
        }
    }

    fn scroll_up_by(&mut self, amount: usize) {
        self.offset = self.offset.saturating_add(amount);
    }

    fn scroll_down_by(&mut self, amount: usize) {
        self.offset = self.offset.saturating_sub(amount);
        if self.offset == 0 {
            self.new_count = 0;
        }
    }

    fn end(&mut self) {
        self.offset = 0;
        self.new_count = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenLayout {
    masthead: Rect,
    tabs: Rect,
    hosts: Rect,
    workspace: Rect,
    composer: Rect,
}

const fn sidebar_width(width: u16) -> u16 {
    if width >= 128 {
        28
    } else if width >= 88 {
        24
    } else {
        20
    }
}

const fn canvas_width(width: u16) -> u16 {
    if width >= 88 {
        width.saturating_sub(sidebar_width(width).saturating_sub(1))
    } else {
        width
    }
}

fn screen_layout(area: Rect, composer_height: u16) -> ScreenLayout {
    let [masthead, tabs, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(area);
    let composer_height = composer_height.max(2).min(body.height);
    let sidebar_width = sidebar_width(area.width);
    let (hosts, workspace, composer) = if area.width >= 88 {
        let [hosts, canvas] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Fill(1)])
            .spacing(Spacing::Overlap(1))
            .areas(body);
        let [workspace, composer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Fill(1), Constraint::Length(composer_height)])
            .spacing(Spacing::Overlap(1))
            .areas(canvas);
        (hosts, workspace, composer)
    } else {
        let [upper, composer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Fill(1), Constraint::Length(composer_height)])
            .spacing(Spacing::Overlap(1))
            .areas(body);
        let [hosts, workspace] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(sidebar_width.min(upper.width.saturating_sub(24))),
                Constraint::Fill(1),
            ])
            .spacing(Spacing::Overlap(1))
            .areas(upper);
        (hosts, workspace, composer)
    };
    ScreenLayout {
        masthead,
        tabs,
        hosts,
        workspace,
        composer,
    }
}

#[derive(Debug)]
struct TraceViewEntry {
    entry: TraceEntry,
    message: Option<Result<Value, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewSnapshot {
    host: HostName,
    step: u64,
    view: View,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptSnapshot {
    host: HostName,
    peer_id: PeerId,
    tier: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopedRunState {
    Waiting {
        observed: usize,
        expected: usize,
    },
    Uniform {
        lifecycle: ExecLifecycle,
        step: Option<u64>,
    },
    Mixed,
}

#[derive(Debug)]
struct ScreenState {
    config: TuiConfig,
    statuses: BTreeMap<HostName, ExecStatus>,
    terminal_lifecycle: Option<ExecLifecycle>,
    agreements: BTreeMap<HostName, (u16, u16)>,
    inspections: BTreeMap<HostName, ExecutionInspection>,
    view_history: BTreeMap<HostName, VecDeque<ViewSnapshot>>,
    view_cursor: usize,
    view_new_count: usize,
    traces: BTreeMap<HostName, Vec<TraceViewEntry>>,
    system_events: Vec<EventFrame>,
    callouts: CalloutQueue,
    receipts: Vec<ReceiptSnapshot>,
    verification_progress: Option<(usize, usize, &'static str)>,
    outcome: Option<Value>,
    host_scope: HostScope,
    host_cursor: usize,
    selected_host: Option<HostName>,
    selected_public_position: Option<u64>,
    trace_cursor: usize,
    complete: bool,
    failure: Option<String>,
    help: bool,
    palette: TuiPalette,
    focus: Focus,
    page: Page,
    selected_crossing: Option<CrossingKey>,
    selected_event: Option<EventKey>,
    program_scroll: u16,
    details_scroll: usize,
    crossings_follow: FollowState,
    trace_follow: FollowState,
    events_follow: FollowState,
    private_page_request: Option<PrivatePageRequest>,
}

impl ScreenState {
    fn new(config: TuiConfig) -> Self {
        let selected_host = config.first_host().map(|host| host.host.clone());
        Self {
            config,
            statuses: BTreeMap::new(),
            terminal_lifecycle: None,
            agreements: BTreeMap::new(),
            inspections: BTreeMap::new(),
            view_history: BTreeMap::new(),
            view_cursor: 0,
            view_new_count: 0,
            traces: BTreeMap::new(),
            system_events: Vec::new(),
            callouts: CalloutQueue::default(),
            receipts: Vec::new(),
            verification_progress: None,
            outcome: None,
            host_scope: HostScope::All,
            host_cursor: 0,
            selected_host,
            selected_public_position: None,
            trace_cursor: 0,
            complete: false,
            failure: None,
            help: false,
            palette: TuiPalette::detect(),
            focus: Focus::Hosts,
            page: Page::default(),
            selected_crossing: None,
            selected_event: None,
            program_scroll: 0,
            details_scroll: 0,
            crossings_follow: FollowState::default(),
            trace_follow: FollowState::default(),
            events_follow: FollowState::default(),
            private_page_request: None,
        }
    }

    fn apply(&mut self, update: RunUpdate) {
        match update {
            RunUpdate::Status { host, status } => {
                if self
                    .statuses
                    .get(&host)
                    .is_some_and(|current| status_is_stale(current, &status))
                {
                    return;
                }
                self.statuses.insert(host, status);
            }
            RunUpdate::Inspection { host, inspection } => {
                if self
                    .inspections
                    .get(&host)
                    .is_some_and(|current| inspection_is_stale(current, &inspection))
                {
                    return;
                }
                let previous_total = self
                    .inspections
                    .get(&host)
                    .map_or(0, |current| current.private_total);
                let added = inspection.private_total.saturating_sub(previous_total);
                self.crossings_follow
                    .items_added(usize::try_from(added).unwrap_or(usize::MAX));
                if !self
                    .statuses
                    .get(&host)
                    .is_some_and(|current| status_is_stale(current, &inspection.status))
                {
                    self.statuses
                        .insert(host.clone(), inspection.status.clone());
                }
                self.inspections.insert(host, *inspection);
                self.reconcile_crossing_selection();
            }
            RunUpdate::Agreement {
                host,
                agreed,
                total,
            } => {
                self.agreements.insert(host, (agreed, total));
            }
            RunUpdate::View { host, step, view } => {
                self.add_view_snapshot(host, step, view);
            }
            RunUpdate::Trace { host, entries } => {
                let current = self.traces.entry(host).or_default();
                if trace_is_stale(current, &entries) {
                    return;
                }
                let old_len = current.len();
                let schema = self.config.message_schema.as_ref();
                *current = entries
                    .into_iter()
                    .map(|entry| {
                        let message = match &entry.event {
                            PublicEvent::SessionStarted { .. } => None,
                            PublicEvent::MessageReceived { msg, .. } => Some(
                                schema
                                    .ok_or_else(|| "program has no message schema".to_owned())
                                    .and_then(|schema| {
                                        schema.decode_json(msg).map_err(|error| error.to_string())
                                    }),
                            ),
                        };
                        TraceViewEntry { entry, message }
                    })
                    .collect();
                let added = current.len().saturating_sub(old_len);
                self.trace_follow.items_added(added);
                self.crossings_follow.items_added(added);
                let steps = self.scoped_trace_steps();
                let trace_len = steps.len();
                let last_step = steps.last().copied();
                if self.trace_cursor >= trace_len {
                    self.trace_cursor = trace_len.saturating_sub(1);
                }
                if self.selected_public_position.is_none() || self.trace_follow.offset == 0 {
                    self.selected_public_position = last_step;
                    self.trace_cursor = trace_len.saturating_sub(1);
                }
                self.reconcile_crossing_selection();
            }
            RunUpdate::SystemEvent { frame } => {
                let visible = frame
                    .host
                    .parse::<HostName>()
                    .is_ok_and(|host| self.host_is_visible(&host));
                let key = EventKey {
                    host: frame.host.clone(),
                    boot_id: frame.boot_id.clone(),
                    seq: frame.seq,
                };
                self.system_events.push(frame);
                if self.system_events.len() > MAX_SYSTEM_EVENTS {
                    self.system_events
                        .drain(..self.system_events.len() - MAX_SYSTEM_EVENTS);
                }
                self.events_follow.items_added(1);
                if visible && (self.selected_event.is_none() || self.events_follow.offset == 0) {
                    self.selected_event = Some(key);
                } else {
                    let keys = events::keys(self);
                    if self
                        .selected_event
                        .as_ref()
                        .is_some_and(|selected| !keys.contains(selected))
                    {
                        self.selected_event = keys.first().cloned();
                    }
                }
            }
            RunUpdate::Callout {
                host,
                exec_id,
                pending_id,
                callout_index,
                name,
                prompt,
                context,
                schema,
                reply,
            } => {
                let mut editor = TextArea::default();
                editor.set_style(Style::default());
                editor.set_cursor_line_style(Style::default());
                editor.set_cursor_style(self.palette.strong().add_modifier(Modifier::REVERSED));
                editor.set_placeholder_text("Enter answer");
                editor.set_placeholder_style(self.palette.muted());
                let callout = PendingCallout {
                    host,
                    exec_id,
                    pending_id,
                    callout_index,
                    name,
                    prompt,
                    context,
                    schema,
                    editor,
                    scroll: 0,
                    validation_error: None,
                    reply,
                };
                if self.callouts.push(callout) && self.callouts.len() == 1 {
                    self.focus = Focus::Composer;
                }
            }
            RunUpdate::ReceiptVerified {
                host,
                peer_id,
                tier,
            } => {
                if let Some(receipt) = self
                    .receipts
                    .iter_mut()
                    .find(|receipt| receipt.host == host && receipt.peer_id == peer_id)
                {
                    receipt.tier = tier;
                } else {
                    self.receipts.push(ReceiptSnapshot {
                        host,
                        peer_id,
                        tier,
                    });
                }
            }
            RunUpdate::VerificationProgress {
                verified,
                total,
                tier,
            } => self.verification_progress = Some((verified, total, tier)),
            RunUpdate::Completed { outcome } => {
                self.complete = true;
                self.outcome = outcome;
                self.terminal_lifecycle = Some(ExecLifecycle::Completed);
            }
            RunUpdate::Stopped { summary } => {
                self.complete = true;
                self.terminal_lifecycle = Some(ExecLifecycle::Aborted);
                self.callouts.clear();
                self.failure = Some(summary);
            }
            RunUpdate::Failed { summary } => {
                self.complete = true;
                self.terminal_lifecycle = Some(ExecLifecycle::Failed);
                self.callouts.clear();
                self.failure = Some(summary);
            }
            RunUpdate::Close => {}
        }
    }

    fn scoped_run_state(&self) -> ScopedRunState {
        if let Some(lifecycle) = self.terminal_lifecycle {
            return ScopedRunState::Uniform {
                lifecycle,
                step: self.step(),
            };
        }
        let hosts = self.scoped_host_names();
        let statuses = hosts
            .iter()
            .filter_map(|host| self.host_status(host))
            .collect::<Vec<_>>();
        if statuses.len() != hosts.len() || statuses.is_empty() {
            return ScopedRunState::Waiting {
                observed: statuses.len(),
                expected: hosts.len(),
            };
        }
        let first = statuses[0];
        if statuses
            .iter()
            .skip(1)
            .any(|status| status.lifecycle() != first.lifecycle() || status.step() != first.step())
        {
            ScopedRunState::Mixed
        } else {
            ScopedRunState::Uniform {
                lifecycle: first.lifecycle(),
                step: first.step(),
            }
        }
    }

    fn scoped_status_label(&self) -> String {
        match self.scoped_run_state() {
            ScopedRunState::Waiting { observed, expected } => {
                format!("waiting {observed}/{expected} Hosts")
            }
            ScopedRunState::Uniform { lifecycle, step } => step.map_or_else(
                || format!("{:?}", lifecycle).to_lowercase(),
                |step| format!("{} step {step}", format!("{:?}", lifecycle).to_lowercase()),
            ),
            ScopedRunState::Mixed => "mixed".to_owned(),
        }
    }

    fn lifecycle(&self) -> ExecLifecycle {
        self.terminal_lifecycle
            .or_else(|| {
                let mut lifecycles = self.statuses.values().map(ExecStatus::lifecycle);
                let first = lifecycles.next()?;
                lifecycles
                    .all(|lifecycle| lifecycle == first)
                    .then_some(first)
            })
            .or_else(|| self.status().map(ExecStatus::lifecycle))
            .unwrap_or(ExecLifecycle::Negotiating)
    }

    fn session_id(&self) -> Option<SessionHash> {
        self.status().and_then(ExecStatus::session_id)
    }

    fn step(&self) -> Option<u64> {
        self.status().and_then(ExecStatus::step)
    }

    fn all_host_names(&self) -> Vec<HostName> {
        let mut hosts = self
            .config
            .hosts
            .iter()
            .map(|host| host.host.clone())
            .collect::<BTreeSet<_>>();
        hosts.extend(self.inspections.keys().cloned());
        hosts.extend(self.statuses.keys().cloned());
        hosts.extend(self.traces.keys().cloned());
        hosts.into_iter().collect()
    }

    fn scoped_host_names(&self) -> Vec<HostName> {
        let available = self.all_host_names();
        match &self.host_scope {
            HostScope::All => available,
            HostScope::One(host) => available
                .into_iter()
                .filter(|candidate| candidate == host)
                .collect(),
            HostScope::Compare { left, right } => available
                .into_iter()
                .filter(|candidate| candidate == left || candidate == right)
                .collect(),
        }
    }

    fn host_is_visible(&self, host: &HostName) -> bool {
        match &self.host_scope {
            HostScope::All => true,
            HostScope::One(selected) => host == selected,
            HostScope::Compare { left, right } => host == left || host == right,
        }
    }

    fn primary_host(&self) -> Option<&HostName> {
        match &self.host_scope {
            HostScope::One(host) => Some(host),
            HostScope::Compare { left, right } => self
                .selected_host
                .as_ref()
                .filter(|selected| *selected == left || *selected == right)
                .or(Some(left)),
            HostScope::All => self
                .selected_host
                .as_ref()
                .or_else(|| self.config.first_host().map(|host| &host.host)),
        }
    }

    fn status(&self) -> Option<&ExecStatus> {
        self.primary_host().and_then(|host| self.statuses.get(host))
    }

    fn scoped_trace_steps(&self) -> Vec<u64> {
        self.scoped_host_names()
            .iter()
            .flat_map(|host| self.host_trace(host).iter().map(|entry| entry.entry.step))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn host_status(&self, host: &HostName) -> Option<&ExecStatus> {
        self.statuses.get(host)
    }

    fn scoped_agreement_label(&self) -> String {
        let agreements = self
            .scoped_host_names()
            .iter()
            .filter_map(|host| self.agreements.get(host).copied())
            .collect::<Vec<_>>();
        let Some(first) = agreements.first().copied() else {
            return "pending".to_owned();
        };
        if agreements.len() != self.scoped_host_names().len() {
            return format!(
                "{}/{} Hosts observed",
                agreements.len(),
                self.scoped_host_names().len()
            );
        }
        if agreements.iter().all(|agreement| *agreement == first) {
            format!("{}/{}", first.0, first.1)
        } else {
            "mixed by Host".to_owned()
        }
    }

    fn host_trace(&self, host: &HostName) -> &[TraceViewEntry] {
        self.traces.get(host).map(Vec::as_slice).unwrap_or_default()
    }

    #[cfg(test)]
    fn set_answer_text(&mut self, text: &str) {
        let callout = self
            .callouts
            .selected_mut()
            .expect("test requires a callout");
        callout.editor = TextArea::default();
        callout.editor.insert_str(text);
        callout.validation_error = None;
    }

    #[cfg(test)]
    fn answer_text(&self) -> String {
        self.callouts
            .selected()
            .map(|callout| callout.editor.lines().join("\n"))
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn answer_validation_error(&self) -> Option<&str> {
        self.callouts
            .selected()
            .and_then(|callout| callout.validation_error.as_deref())
    }

    #[cfg(test)]
    fn active_view(&self) -> Option<&View> {
        self.active_view_snapshot().map(|snapshot| &snapshot.view)
    }

    fn active_view_snapshot(&self) -> Option<&ViewSnapshot> {
        let host = self.selected_host.as_ref()?;
        self.view_history
            .get(host)
            .and_then(|history| history.get(self.view_cursor))
    }

    fn is_live_view(&self) -> bool {
        let count = self.selected_host.as_ref().map_or(0, |host| {
            self.view_history.get(host).map_or(0, VecDeque::len)
        });
        count == 0 || self.view_cursor + 1 >= count
    }

    fn add_view_snapshot(&mut self, host: HostName, step: u64, view: View) {
        let was_live = self.is_live_view();
        if self.selected_host.is_none() {
            self.selected_host = Some(host.clone());
        }
        let history = self.view_history.entry(host.clone()).or_default();
        if let Some(existing) = history.iter_mut().find(|snapshot| snapshot.step == step) {
            existing.view = view;
            return;
        }
        history.push_back(ViewSnapshot {
            host: host.clone(),
            step,
            view,
        });
        history
            .make_contiguous()
            .sort_by_key(|snapshot| snapshot.step);
        while history.len() > MAX_VIEW_HISTORY {
            history.pop_front();
        }
        if self.selected_host.as_ref() == Some(&host) && was_live {
            self.view_cursor = self.view_count().saturating_sub(1);
            self.view_new_count = 0;
            self.program_scroll = 0;
        } else if self.selected_host.as_ref() == Some(&host) {
            self.view_new_count = self.view_new_count.saturating_add(1);
        }
    }

    fn view_count(&self) -> usize {
        self.selected_host.as_ref().map_or(0, |host| {
            self.view_history.get(host).map_or(0, VecDeque::len)
        })
    }

    fn select_host(&mut self, host: HostName) {
        if self.selected_host.as_ref() == Some(&host) {
            return;
        }
        self.selected_host = Some(host.clone());
        self.view_cursor = self.view_count().saturating_sub(1);
        self.view_new_count = 0;
        self.program_scroll = 0;
    }

    fn previous_view(&mut self) {
        if self.view_count() != 0 {
            self.view_cursor = self.view_cursor.saturating_sub(1);
            self.program_scroll = 0;
        }
    }

    fn next_view(&mut self) {
        if self.view_count() != 0 {
            self.view_cursor = (self.view_cursor + 1).min(self.view_count() - 1);
            if self.is_live_view() {
                self.view_new_count = 0;
            }
            self.program_scroll = 0;
        }
    }

    fn follow_live(&mut self) {
        self.view_cursor = self.view_count().saturating_sub(1);
        self.view_new_count = 0;
        self.program_scroll = 0;
    }

    fn focus_next(&mut self) {
        match (self.focus, self.page) {
            (Focus::Hosts, _) => {
                self.focus = Focus::Workspace;
                self.page.close_inspector();
            }
            (Focus::Composer, _) => self.focus = Focus::Hosts,
            (Focus::Workspace, Page::Overview(pane)) => {
                if pane == OverviewPane::SystemEvents {
                    self.focus = if self.callouts.is_empty() {
                        Focus::Hosts
                    } else {
                        Focus::Composer
                    };
                } else {
                    self.page.set_pane(pane.next());
                }
            }
            (
                Focus::Workspace,
                Page::Detail {
                    region: DetailRegion::Records,
                    ..
                },
            ) => {
                self.page.open_inspector();
                self.details_scroll = 0;
            }
            (
                Focus::Workspace,
                Page::Detail {
                    region: DetailRegion::Inspector,
                    ..
                },
            ) => {
                self.page.close_inspector();
                self.focus = if self.callouts.is_empty() {
                    Focus::Hosts
                } else {
                    Focus::Composer
                };
            }
        }
    }

    fn focus_previous(&mut self) {
        match (self.focus, self.page) {
            (Focus::Hosts, _) if !self.callouts.is_empty() => {
                self.focus = Focus::Composer;
                self.page.close_inspector();
            }
            (Focus::Hosts, Page::Overview(_)) | (Focus::Composer, Page::Overview(_)) => {
                self.focus = Focus::Workspace;
                self.page.set_pane(OverviewPane::SystemEvents);
            }
            (Focus::Hosts, Page::Detail { .. }) => {
                self.focus = Focus::Workspace;
                self.page.open_inspector();
                self.details_scroll = 0;
            }
            (Focus::Composer, _) => self.focus = Focus::Workspace,
            (Focus::Workspace, Page::Overview(pane)) => {
                if pane == OverviewPane::Program && !self.callouts.is_empty() {
                    self.focus = Focus::Composer;
                } else {
                    self.page.set_pane(pane.previous());
                }
            }
            (
                Focus::Workspace,
                Page::Detail {
                    region: DetailRegion::Inspector,
                    ..
                },
            ) => {
                self.page.close_inspector();
            }
            (
                Focus::Workspace,
                Page::Detail {
                    region: DetailRegion::Records,
                    ..
                },
            ) => {
                if self.callouts.is_empty() {
                    self.page.open_inspector();
                    self.details_scroll = 0;
                } else {
                    self.focus = Focus::Composer;
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<UiExit> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(UiExit::Cancelled);
        }
        if self.complete && key.code == KeyCode::Char('q') {
            return Some(UiExit::Completed);
        }
        match key.code {
            KeyCode::Char('q') if !self.in_insert_mode() => Some(UiExit::Cancelled),
            KeyCode::Char('?') if !self.in_insert_mode() => {
                self.help = !self.help;
                None
            }
            KeyCode::Esc => {
                if self.help {
                    self.help = false;
                } else if self.page.inspector().is_some() {
                    self.page.close_inspector();
                } else {
                    self.focus = Focus::Hosts;
                    self.page.overview();
                }
                None
            }
            KeyCode::Tab => {
                self.focus_next();
                None
            }
            KeyCode::PageUp
                if self.in_insert_mode() && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(callout) = self.callouts.selected_mut() {
                    callout.scroll = callout.scroll.saturating_sub(4);
                }
                None
            }
            KeyCode::PageDown
                if self.in_insert_mode() && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(callout) = self.callouts.selected_mut() {
                    callout.scroll = callout.scroll.saturating_add(4);
                }
                None
            }
            KeyCode::Char('[') if self.focus == Focus::Composer && self.callouts.len() > 1 => {
                self.callouts.previous();
                None
            }
            KeyCode::Char(']') if self.focus == Focus::Composer && self.callouts.len() > 1 => {
                self.callouts.next();
                None
            }
            KeyCode::Char('<')
                if !self.in_insert_mode() && self.page.view() == WorkspaceView::Wasm =>
            {
                self.request_private_page(false);
                None
            }
            KeyCode::Char('>')
                if !self.in_insert_mode() && self.page.view() == WorkspaceView::Wasm =>
            {
                self.request_private_page(true);
                None
            }
            KeyCode::Char('a' | 'A') if !self.in_insert_mode() => {
                self.host_scope = HostScope::All;
                self.host_cursor = 0;
                self.reconcile_scope_selection();
                self.details_scroll = 0;
                None
            }
            KeyCode::Char('c' | 'C') if !self.in_insert_mode() => {
                self.compare_hosts();
                self.details_scroll = 0;
                None
            }
            KeyCode::Char('i' | 'I') if !self.callouts.is_empty() && !self.in_insert_mode() => {
                self.page.close_inspector();
                self.focus = Focus::Composer;
                None
            }
            KeyCode::BackTab => {
                self.focus_previous();
                None
            }
            KeyCode::Char(number) if !self.in_insert_mode() => {
                if let Some(view) = WorkspaceView::from_number(number) {
                    self.page.select(view);
                    self.focus = Focus::Workspace;
                    self.page.close_inspector();
                    self.details_scroll = 0;
                }
                None
            }
            KeyCode::Left
                if self.focus == Focus::Workspace && self.page.view() == WorkspaceView::Program =>
            {
                self.previous_view();
                None
            }
            KeyCode::Right
                if self.focus == Focus::Workspace && self.page.view() == WorkspaceView::Program =>
            {
                self.next_view();
                None
            }
            KeyCode::Up if !self.in_insert_mode() => {
                if self.focus == Focus::Hosts {
                    self.move_host_cursor(1, false);
                } else {
                    self.scroll_focused_up(1);
                }
                None
            }
            KeyCode::Down if !self.in_insert_mode() => {
                if self.focus == Focus::Hosts {
                    self.move_host_cursor(1, true);
                } else {
                    self.scroll_focused_down(1);
                }
                None
            }
            KeyCode::PageUp if !self.in_insert_mode() => {
                if self.focus == Focus::Hosts {
                    self.move_host_cursor(8, false);
                } else {
                    self.scroll_focused_up(8);
                }
                None
            }
            KeyCode::PageDown if !self.in_insert_mode() => {
                if self.focus == Focus::Hosts {
                    self.move_host_cursor(8, true);
                } else {
                    self.scroll_focused_down(8);
                }
                None
            }
            KeyCode::Home if !self.in_insert_mode() => {
                if self.focus == Focus::Hosts {
                    self.host_cursor = 0;
                } else {
                    self.home_focused_scroll();
                }
                None
            }
            KeyCode::End if !self.in_insert_mode() => {
                if self.focus == Focus::Hosts {
                    self.host_cursor = self.config.host_count();
                } else {
                    self.end_focused_scroll();
                }
                if self.focus == Focus::Workspace && self.page.view() == WorkspaceView::Wasm {
                    self.request_private_tail();
                }
                None
            }
            KeyCode::Enter => {
                if self.focus == Focus::Composer && !self.callouts.is_empty() {
                    self.submit_answer();
                } else if self.focus == Focus::Hosts {
                    self.activate_host_cursor();
                } else if self.focus == Focus::Workspace
                    && self.page.view() == WorkspaceView::Overview
                {
                    self.page.open_detail();
                } else if self.focus == Focus::Workspace {
                    self.page.open_inspector();
                    self.details_scroll = 0;
                }
                None
            }
            _ if self.in_insert_mode() => {
                if let Some(callout) = self.callouts.selected_mut() {
                    callout.editor.input(key);
                    callout.validation_error = None;
                }
                None
            }
            _ => None,
        }
    }

    fn in_insert_mode(&self) -> bool {
        !self.callouts.is_empty() && self.focus == Focus::Composer
    }

    fn interaction_view(&self) -> WorkspaceView {
        if self.page.view() == WorkspaceView::Overview {
            self.page.pane().view()
        } else {
            self.page.view()
        }
    }

    fn move_host_cursor(&mut self, amount: usize, forward: bool) {
        let last = self.config.host_count();
        self.host_cursor = if forward {
            self.host_cursor.saturating_add(amount).min(last)
        } else {
            self.host_cursor.saturating_sub(amount)
        };
    }

    fn activate_host_cursor(&mut self) {
        if self.host_cursor == 0 {
            self.host_scope = HostScope::All;
        } else if let Some(host) = self
            .config
            .hosts
            .get(self.host_cursor - 1)
            .map(|host| host.host.clone())
        {
            self.host_scope = HostScope::One(host.clone());
            self.select_host(host);
        }
        self.reconcile_scope_selection();
        self.details_scroll = 0;
    }

    fn compare_hosts(&mut self) {
        let hosts = self.all_host_names();
        if hosts.len() < 2 {
            return;
        }
        let left = self
            .primary_host()
            .cloned()
            .unwrap_or_else(|| hosts[0].clone());
        let left_index = hosts.iter().position(|host| host == &left).unwrap_or(0);
        let right = hosts[(left_index + 1) % hosts.len()].clone();
        self.host_cursor = self
            .config
            .hosts
            .iter()
            .position(|host| host.host == left)
            .map_or(0, |index| index + 1);
        self.host_scope = HostScope::Compare { left, right };
        self.reconcile_scope_selection();
    }

    fn request_private_page(&mut self, newer: bool) {
        let Some(host) = self.primary_host().cloned() else {
            return;
        };
        let Some(inspection) = self.inspections.get(&host) else {
            return;
        };
        let from = if newer {
            if let Some(next) = inspection.private_next {
                Some(next)
            } else if inspection.private_from > 0 {
                None
            } else {
                return;
            }
        } else if inspection.private_from == 0 {
            return;
        } else {
            Some(
                inspection
                    .private_from
                    .saturating_sub(u64::from(PRIVATE_INSPECTION_LIMIT)),
            )
        };
        self.private_page_request = Some(PrivatePageRequest { host, from });
    }

    fn request_private_tail(&mut self) {
        if let Some(host) = self.primary_host().cloned() {
            self.private_page_request = Some(PrivatePageRequest { host, from: None });
        }
    }

    fn reconcile_scope_selection(&mut self) {
        let scoped = self.scoped_host_names();
        if !self
            .selected_host
            .as_ref()
            .is_some_and(|host| scoped.contains(host))
        {
            self.selected_host = scoped.first().cloned();
        }
        let steps = self.scoped_trace_steps();
        let trace_len = steps.len();
        let selected_step = steps
            .get(self.trace_cursor.min(trace_len.saturating_sub(1)))
            .copied();
        self.trace_cursor = self.trace_cursor.min(trace_len.saturating_sub(1));
        self.selected_public_position = selected_step;
        let event_keys = events::keys(self);
        if !self
            .selected_event
            .as_ref()
            .is_some_and(|selected| event_keys.contains(selected))
        {
            self.selected_event = event_keys.last().cloned();
        }
        self.reconcile_crossing_selection();
    }

    fn select_public_step(&mut self, step: Option<u64>) {
        self.selected_public_position = step;
        let Some(step) = step else {
            return;
        };
        if let Some(host) = &self.selected_host
            && let Some(history) = self.view_history.get(host)
        {
            let program_step = step.saturating_add(1);
            if let Some(index) = history
                .iter()
                .position(|snapshot| snapshot.step == program_step)
                .or_else(|| history.iter().position(|snapshot| snapshot.step == step))
            {
                self.view_cursor = index;
                self.view_new_count = history.len().saturating_sub(index + 1);
                self.program_scroll = 0;
            }
        }
        let public = CrossingKey::Public { step };
        if wasm::keys(self).contains(&public) {
            self.selected_crossing = Some(public);
        }
    }

    fn move_host_selection(&mut self, amount: usize, forward: bool) {
        let hosts = self.scoped_host_names();
        let Some(last) = hosts.len().checked_sub(1) else {
            return;
        };
        let current = self
            .selected_host
            .as_ref()
            .and_then(|selected| hosts.iter().position(|host| host == selected))
            .unwrap_or(0);
        let selected = if forward {
            current.saturating_add(amount).min(last)
        } else {
            current.saturating_sub(amount)
        };
        self.select_host(hosts[selected].clone());
    }

    fn move_crossing_selection(&mut self, amount: usize, forward: bool) {
        let keys = wasm::keys(self);
        let Some(last) = keys.len().checked_sub(1) else {
            self.selected_crossing = None;
            return;
        };
        let current = self
            .selected_crossing
            .as_ref()
            .and_then(|selected| keys.iter().position(|key| key == selected))
            .unwrap_or(last);
        let selected = if forward {
            current.saturating_add(amount).min(last)
        } else {
            current.saturating_sub(amount)
        };
        self.selected_crossing = Some(keys[selected].clone());
        self.crossings_follow.offset = last - selected;
        if selected == last {
            self.crossings_follow.end();
        }
    }

    fn reconcile_crossing_selection(&mut self) {
        let keys = wasm::keys(self);
        let selection_is_visible = self
            .selected_crossing
            .as_ref()
            .is_some_and(|selected| keys.contains(selected));
        if !selection_is_visible || self.crossings_follow.offset == 0 {
            self.selected_crossing = keys.last().cloned();
        }
    }

    fn move_event_selection(&mut self, amount: usize, forward: bool) {
        let keys = events::keys(self);
        let Some(last) = keys.len().checked_sub(1) else {
            self.selected_event = None;
            return;
        };
        let current = self
            .selected_event
            .as_ref()
            .and_then(|selected| keys.iter().position(|key| key == selected))
            .unwrap_or(last);
        let selected = if forward {
            current.saturating_add(amount).min(last)
        } else {
            current.saturating_sub(amount)
        };
        self.selected_event = Some(keys[selected].clone());
        self.events_follow.offset = last - selected;
        if selected == last {
            self.events_follow.end();
        }
    }

    fn on_paste(&mut self, text: String) {
        if let Some(callout) = self
            .callouts
            .selected_mut()
            .filter(|_| self.focus == Focus::Composer)
        {
            callout.editor.insert_str(text);
            callout.validation_error = None;
        }
    }

    fn scroll_focused_up(&mut self, amount: usize) {
        if self.focus == Focus::Composer {
            return;
        }
        if self.page.region() == DetailRegion::Inspector {
            if self.interaction_view() == WorkspaceView::Program {
                self.program_scroll = self
                    .program_scroll
                    .saturating_sub(u16::try_from(amount).unwrap_or(u16::MAX));
            } else {
                self.details_scroll = self.details_scroll.saturating_sub(amount);
            }
            return;
        }
        match self.interaction_view() {
            WorkspaceView::Negotiation => self.move_host_selection(amount, false),
            WorkspaceView::Program => self.move_host_selection(amount, false),
            WorkspaceView::Wasm => self.move_crossing_selection(amount, false),
            WorkspaceView::PublicTrace => {
                self.trace_cursor = self.trace_cursor.saturating_sub(amount);
                let step = self.scoped_trace_steps().get(self.trace_cursor).copied();
                self.select_public_step(step);
                self.trace_follow.scroll_up_by(amount);
            }
            WorkspaceView::SystemEvents => self.move_event_selection(amount, false),
            WorkspaceView::Overview => unreachable!("Overview resolves to its focused pane"),
        }
    }

    fn scroll_focused_down(&mut self, amount: usize) {
        if self.focus == Focus::Composer {
            return;
        }
        if self.page.region() == DetailRegion::Inspector {
            if self.interaction_view() == WorkspaceView::Program {
                self.program_scroll = self
                    .program_scroll
                    .saturating_add(u16::try_from(amount).unwrap_or(u16::MAX));
            } else {
                self.details_scroll = self.details_scroll.saturating_add(amount);
            }
            return;
        }
        match self.interaction_view() {
            WorkspaceView::Negotiation => self.move_host_selection(amount, true),
            WorkspaceView::Program => self.move_host_selection(amount, true),
            WorkspaceView::Wasm => self.move_crossing_selection(amount, true),
            WorkspaceView::PublicTrace => {
                self.trace_cursor = self
                    .trace_cursor
                    .saturating_add(amount)
                    .min(self.scoped_trace_steps().len().saturating_sub(1));
                let step = self.scoped_trace_steps().get(self.trace_cursor).copied();
                self.select_public_step(step);
                self.trace_follow.scroll_down_by(amount);
            }
            WorkspaceView::SystemEvents => self.move_event_selection(amount, true),
            WorkspaceView::Overview => unreachable!("Overview resolves to its focused pane"),
        }
    }

    fn home_focused_scroll(&mut self) {
        if self.focus == Focus::Composer {
            return;
        }
        if self.page.region() == DetailRegion::Inspector {
            if self.interaction_view() == WorkspaceView::Program {
                self.program_scroll = 0;
            } else {
                self.details_scroll = 0;
            }
            return;
        }
        match self.interaction_view() {
            WorkspaceView::Negotiation => {
                if let Some(host) = self.scoped_host_names().first().cloned() {
                    self.select_host(host);
                }
            }
            WorkspaceView::Program => self.program_scroll = 0,
            WorkspaceView::Wasm => {
                self.selected_crossing = wasm::keys(self).first().cloned();
                self.crossings_follow.offset = wasm::row_count(self).saturating_sub(1);
            }
            WorkspaceView::PublicTrace => {
                self.trace_cursor = 0;
                let step = self.scoped_trace_steps().first().copied();
                self.select_public_step(step);
                self.trace_follow = FollowState::default();
            }
            WorkspaceView::SystemEvents => {
                self.selected_event = events::keys(self).first().cloned();
                self.events_follow.offset = events::row_count(self).saturating_sub(1);
            }
            WorkspaceView::Overview => unreachable!("Overview resolves to its focused pane"),
        }
    }

    fn end_focused_scroll(&mut self) {
        if self.focus == Focus::Composer {
            return;
        }
        if self.page.region() == DetailRegion::Inspector {
            if self.interaction_view() == WorkspaceView::Program {
                self.program_scroll = u16::MAX;
            } else {
                self.details_scroll = usize::MAX;
            }
            return;
        }
        match self.interaction_view() {
            WorkspaceView::Negotiation => {
                if let Some(host) = self.scoped_host_names().last().cloned() {
                    self.select_host(host);
                }
            }
            WorkspaceView::Program => self.follow_live(),
            WorkspaceView::Wasm => {
                self.selected_crossing = wasm::keys(self).last().cloned();
                self.crossings_follow.end();
            }
            WorkspaceView::PublicTrace => {
                let steps = self.scoped_trace_steps();
                self.trace_cursor = steps.len().saturating_sub(1);
                self.select_public_step(steps.last().copied());
                self.trace_follow.end();
            }
            WorkspaceView::SystemEvents => {
                self.selected_event = events::keys(self).last().cloned();
                self.events_follow.end();
            }
            WorkspaceView::Overview => unreachable!("Overview resolves to its focused pane"),
        }
    }

    fn submit_answer(&mut self) {
        let Some(callout) = self.callouts.selected_mut() else {
            return;
        };
        let input = callout.editor.lines().join("\n");
        let value = answer::scalar(input.trim());
        let validator = match jsonschema::validator_for(&callout.schema) {
            Ok(validator) => validator,
            Err(error) => {
                callout.validation_error = Some(format!("invalid answer schema: {error}"));
                return;
            }
        };
        if let Err(error) = validator.validate(&value) {
            let path = error.instance_path().as_str();
            let path = if path.is_empty() { "$" } else { path };
            callout.validation_error = Some(format!("{path}: {error}"));
            return;
        }
        let callout = self
            .callouts
            .remove_selected()
            .expect("callout checked above");
        let _ = callout.reply.send(value);
        if !self.callouts.is_empty() {
            self.focus = Focus::Composer;
        } else {
            self.focus = Focus::Workspace;
        }
    }
}

fn lifecycle_rank(lifecycle: ExecLifecycle) -> u8 {
    match lifecycle {
        ExecLifecycle::Negotiating => 0,
        ExecLifecycle::Activating => 1,
        ExecLifecycle::Waiting => 2,
        ExecLifecycle::Active => 3,
        ExecLifecycle::Completed
        | ExecLifecycle::Aborted
        | ExecLifecycle::Incomplete
        | ExecLifecycle::Failed => 4,
    }
}

fn status_is_stale(current: &ExecStatus, incoming: &ExecStatus) -> bool {
    if current.exec_id != incoming.exec_id
        || current.program_id != incoming.program_id
        || current
            .negotiation_id
            .is_some_and(|id| incoming.negotiation_id != Some(id))
        || current
            .session_id()
            .is_some_and(|id| incoming.session_id() != Some(id))
    {
        return true;
    }
    let current_lifecycle = current.lifecycle();
    let incoming_lifecycle = incoming.lifecycle();
    if current_lifecycle.is_terminal() && incoming_lifecycle != current_lifecycle {
        return true;
    }
    if lifecycle_rank(incoming_lifecycle) < lifecycle_rank(current_lifecycle) {
        return true;
    }
    match (current.session(), incoming.session()) {
        (Some(current), Some(incoming)) => incoming.step < current.step,
        (Some(_), None) => true,
        _ => false,
    }
}

fn inspection_is_stale(current: &ExecutionInspection, incoming: &ExecutionInspection) -> bool {
    status_is_stale(&current.status, &incoming.status)
        || incoming.private_total < current.private_total
        || matches!(
            (&current.activation, &incoming.activation),
            (Some(_), None)
                | (
                    Some(ActivationInspection {
                        state: arena0_client::api::ActivationInspectionState::Committed,
                        ..
                    }),
                    Some(ActivationInspection {
                        state: arena0_client::api::ActivationInspectionState::Prepared,
                        ..
                    })
                )
        )
}

fn trace_is_stale(current: &[TraceViewEntry], incoming: &[TraceEntry]) -> bool {
    let Some(current_last) = current.last() else {
        return false;
    };
    let Some(incoming_last) = incoming.last() else {
        return true;
    };
    incoming_last.step < current_last.entry.step
        || (incoming_last.step == current_last.entry.step && incoming.len() < current.len())
}

async fn run_screen(
    config: TuiConfig,
    mut updates: mpsc::Receiver<RunUpdate>,
    width: watch::Sender<u16>,
    private_page: watch::Sender<Option<PrivatePageRequest>>,
    cancel: watch::Sender<Option<String>>,
) -> anyhow::Result<UiExit> {
    let result = run_screen_inner(config, &mut updates, width, private_page, &cancel).await;
    if let Err(error) = &result {
        cancel.send_replace(Some(format!("{error:#}")));
    }
    result
}

async fn run_screen_inner(
    config: TuiConfig,
    updates: &mut mpsc::Receiver<RunUpdate>,
    width: watch::Sender<u16>,
    private_page: watch::Sender<Option<PrivatePageRequest>>,
    cancel: &watch::Sender<Option<String>>,
) -> anyhow::Result<UiExit> {
    let (mut terminal, _restore) = crate::terminal::enter()?;
    let mut events = EventStream::new();
    let mut state = ScreenState::new(config);

    loop {
        let area = terminal.size().context("read terminal size")?;
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            publish_width(&width, area.width.saturating_sub(2).max(1));
            terminal
                .draw(|frame| render_too_small(frame, area.into()))
                .context("draw small-terminal run TUI")?;
            tokio::select! {
                update = updates.recv() => {
                    let Some(update) = update else { return Ok(UiExit::Closed); };
                    if matches!(update, RunUpdate::Close) {
                        return Ok(UiExit::Closed);
                    }
                    state.apply(update);
                }
                event = events.next() => {
                    let event = event.ok_or_else(|| anyhow!("terminal event stream closed"))??;
                    if let Event::Key(key) = event {
                        if let Some(exit) = state.on_key(key) {
                            if exit == UiExit::Cancelled {
                                cancel.send_replace(Some("run stopped from the TUI".to_owned()));
                            }
                            return Ok(exit);
                        }
                        publish_private_page(&mut state, &private_page);
                    } else if let Event::Paste(text) = event {
                        state.on_paste(text);
                    }
                }
            }
            continue;
        }
        publish_width(&width, program_view_width(&state, area.into()));
        terminal
            .draw(|frame| render(frame, &state))
            .context("draw run TUI")?;

        tokio::select! {
            update = updates.recv() => {
                let Some(update) = update else { return Ok(UiExit::Closed); };
                if matches!(update, RunUpdate::Close) {
                    return Ok(UiExit::Closed);
                }
                state.apply(update);
            }
            event = events.next() => {
                let event = event.ok_or_else(|| anyhow!("terminal event stream closed"))??;
                match event {
                    Event::Key(key) => {
                        if let Some(exit) = state.on_key(key) {
                            if exit == UiExit::Cancelled {
                                cancel.send_replace(Some("run stopped from the TUI".to_owned()));
                            }
                            return Ok(exit);
                        }
                        publish_private_page(&mut state, &private_page);
                    }
                    Event::Resize(_, _) => {}
                    Event::Paste(text) => state.on_paste(text),
                    _ => {}
                }
            }
        }
    }
}

fn publish_private_page(
    state: &mut ScreenState,
    requests: &watch::Sender<Option<PrivatePageRequest>>,
) {
    if let Some(request) = state.private_page_request.take() {
        requests.send_replace(Some(request));
    }
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    let message = format!(
        "Terminal is too small for the execution observatory.\nResize to at least {MIN_WIDTH} columns by {MIN_HEIGHT} rows.\nThe run remains active while waiting."
    );
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" arena0 ")),
        area,
    );
}

fn render(frame: &mut Frame<'_>, state: &ScreenState) {
    let area = frame.area();
    let canvas_width = canvas_width(area.width);
    let requested_composer = composer_height(state, canvas_width);
    let composer_capacity = Rect::new(0, 0, canvas_width, area.height.saturating_sub(2));
    if !state.callouts.is_empty() && composer_overflows(state, composer_capacity) {
        render_composer(frame, state, area);
        return;
    }
    let layout = screen_layout(area, requested_composer);

    render_masthead(frame, state, layout.masthead);
    render_tab_bar(frame, state, layout.tabs);
    render_hosts(frame, state, layout.hosts);
    render_workspace(frame, state, layout.workspace);
    render_composer(frame, state, layout.composer);

    if state.help {
        let overlay = centered(area, 70, 16);
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new(vec![
                help_line(state, "Tab / Shift-Tab", "Move focus inside this view"),
                help_line(state, "1–6", "Switch workspace view"),
                help_line(
                    state,
                    "↑/↓ in Hosts",
                    "Move through All Hosts and Host rows",
                ),
                help_line(state, "Enter in Hosts", "Use the highlighted Host scope"),
                help_line(state, "a / c", "View all Hosts or compare two Hosts"),
                help_line(state, "↑/↓  PgUp/PgDn", "Scroll the focused pane"),
                help_line(state, "←/→ (program)", "Browse state history"),
                help_line(state, "</> (Wasm)", "Load older or newer private records"),
                help_line(state, "End", "Follow the live state/tail"),
                help_line(state, "Enter", "Open a pane, inspect a row, or submit"),
                help_line(state, "Esc", "Return to Overview or close help"),
                help_line(state, "i", "Return to a pending answer"),
                help_line(state, "?", "Toggle help"),
                help_line(state, "q (workspace)  Ctrl-C", "Stop the run"),
            ])
            .block(panel(" HELP ", state, true)),
            overlay,
        );
    }
}

fn render_tab_bar(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let titles = if area.width >= 78 {
        vec![
            "1 Overview",
            "2 Negotiation",
            "3 Program",
            "4 Trace",
            "5 Wasm",
            "6 Events",
        ]
    } else {
        vec!["1 Ovr", "2 Nego", "3 Prog", "4 Trace", "5 Wasm", "6 Events"]
    };
    frame.render_widget(
        Tabs::new(titles)
            .select(state.page.view().index())
            .style(state.palette.muted())
            .highlight_style(
                state
                    .palette
                    .strong()
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )
            .divider(" ")
            .padding(" ", " "),
        area,
    );
}

fn help_line(state: &ScreenState, key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<22}"), state.palette.strong()),
        Span::raw(description),
    ])
}

fn render_masthead(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let (left, right) = if area.width >= 72 {
        let [left, right] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(32)])
            .areas(area);
        (left, Some(right))
    } else {
        (area, None)
    };
    let mut masthead = vec![
        Span::styled("arena0", state.palette.strong()),
        Span::raw("  "),
        Span::styled(state.config.program.clone(), state.palette.emphasis()),
        Span::styled("    ", state.palette.muted()),
    ];
    masthead.extend(lifecycle_line(state, true).spans);
    frame.render_widget(Paragraph::new(Line::from(masthead)), left);
    if let Some(right) = right {
        frame.render_widget(
            Paragraph::new(format!(
                "LOCAL    {} Hosts    {}",
                state.config.host_count(),
                control_label(state)
            ))
            .style(state.palette.muted())
            .alignment(Alignment::Right),
            right,
        );
    }
}

fn control_label(state: &ScreenState) -> String {
    let humans = state.config.human_hosts().collect::<Vec<_>>();
    match humans.as_slice() {
        [] => "YOU NONE".to_owned(),
        [host] => format!("YOU {}", host.host),
        hosts if hosts.len() == state.config.host_count() => "YOU ALL".to_owned(),
        hosts => format!("YOU {}/{}", hosts.len(), state.config.host_count()),
    }
}

fn host_scope_label(state: &ScreenState) -> String {
    match &state.host_scope {
        HostScope::All => "ALL HOSTS".to_owned(),
        HostScope::One(host) => host.to_string(),
        HostScope::Compare { left, right } => format!("COMPARE  {left} ↔ {right}"),
    }
}

fn render_hosts(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let focused = state.focus == Focus::Hosts;
    let aggregate = format!(
        "{} Hosts\n{}    agreement {}",
        state.config.host_count(),
        format!("{:?}", state.lifecycle()).to_uppercase(),
        state.scoped_agreement_label()
    );
    let mut rows = vec![Row::new([Cell::from("ALL"), Cell::from(aggregate)]).height(2)];
    rows.extend(state.config.hosts.iter().map(|host| {
        let pending = state.callouts.for_host(&host.host);
        let side = match &state.host_scope {
            HostScope::Compare { left, .. } if left == &host.host => "A",
            HostScope::Compare { right, .. } if right == &host.host => "B",
            _ => "",
        };
        let perspective = if state.selected_host.as_ref() == Some(&host.host) {
            ">"
        } else {
            ""
        };
        let marker = format!("{side}{perspective}");
        let controller = match &host.driver {
            TuiDriver::Human if pending => "YOU    INPUT".to_owned(),
            TuiDriver::Human => "YOU".to_owned(),
            TuiDriver::Builtin(strategy) => strategy.to_ascii_uppercase(),
            TuiDriver::Agent => "AGENT".to_owned(),
        };
        let lifecycle =
            state
                .host_status(&host.host)
                .map_or("WAITING", |status| match status.lifecycle() {
                    ExecLifecycle::Negotiating => "NEGOTIATING",
                    ExecLifecycle::Activating => "ACTIVATING",
                    ExecLifecycle::Waiting => "WAITING",
                    ExecLifecycle::Active => "ACTIVE",
                    ExecLifecycle::Completed => "COMPLETE",
                    ExecLifecycle::Aborted => "ABORTED",
                    ExecLifecycle::Incomplete => "INCOMPLETE",
                    ExecLifecycle::Failed => "FAILED",
                });
        let step = state
            .host_status(&host.host)
            .and_then(ExecStatus::step)
            .map_or_else(|| "-".to_owned(), |step| step.to_string());
        let agreement = state.agreements.get(&host.host).map_or_else(
            || "-".to_owned(),
            |(agreed, total)| format!("{agreed}/{total}"),
        );
        Row::new([
            Cell::from(marker),
            Cell::from(format!(
                "{}  {controller}\n{lifecycle} #{step}  {agreement}",
                host.host
            )),
        ])
        .height(2)
        .style(if host.driver == TuiDriver::Human || pending {
            state.palette.input()
        } else {
            Style::default()
        })
    }));
    let mut table_state = TableState::default().with_selected(Some(state.host_cursor));
    let table = Table::new(rows, [Constraint::Length(3), Constraint::Fill(1)])
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED))
        .block(panel(
            format!("Hosts  {}", host_scope_label(state)),
            state,
            focused,
        ));
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn render_workspace(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let focused = state.focus == Focus::Workspace;
    match state.page.view() {
        WorkspaceView::Overview => render_overview(frame, state, area),
        WorkspaceView::Negotiation => negotiation::render(frame, state, area, focused),
        WorkspaceView::Program => program::render(frame, state, area, focused),
        WorkspaceView::PublicTrace => trace::render(frame, state, area, focused),
        WorkspaceView::Wasm => wasm::render(frame, state, area, focused),
        WorkspaceView::SystemEvents => events::render(frame, state, area, focused),
    }
}

fn render_composer(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    if let Some(callout) = state.callouts.selected() {
        let [composer, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(1)])
            .areas(area);
        let details_paragraph =
            Paragraph::new(callout_detail_lines(state, callout)).wrap(Wrap { trim: false });
        let inner_width = composer.width.saturating_sub(2).max(1);
        let details_height =
            u16::try_from(details_paragraph.line_count(inner_width)).unwrap_or(u16::MAX);
        let input_height = u16::try_from(callout.editor.lines().len().max(1)).unwrap_or(u16::MAX);
        let inner_height = composer.height.saturating_sub(2);
        let overflow = details_height.saturating_add(input_height) > inner_height;
        let overflow_label = if overflow { "  TERMINAL TOO SHORT" } else { "" };
        let block = Block::default()
            .borders(Borders::ALL)
            .merge_borders(MergeStrategy::Exact)
            .title(Span::styled(
                format!(
                    " YOUR TURN  {}  REQUEST {} OF {}{overflow_label} ",
                    callout.host,
                    state.callouts.position(),
                    state.callouts.len()
                ),
                state.palette.input().add_modifier(Modifier::REVERSED),
            ))
            .border_style(if state.focus == Focus::Composer {
                state.palette.input()
            } else {
                state.palette.muted()
            });
        let inner = block.inner(composer);
        frame.render_widget(block, composer);
        let visible_input_height = input_height.min(inner.height);
        let visible_details_height = inner.height.saturating_sub(visible_input_height);
        let [details, input] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(visible_details_height),
                Constraint::Length(visible_input_height),
            ])
            .areas(inner);
        frame.render_widget(details_paragraph.scroll((callout.scroll, 0)), details);
        frame.render_widget(&callout.editor, input);
        frame.render_widget(
            Paragraph::new(key_hints(state, area.width, overflow)).style(state.palette.muted()),
            footer,
        );
    } else {
        let [status, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .areas(area);
        let (content, style) = if let Some(failure) = &state.failure {
            (format!("Failed    {failure}"), state.palette.error())
        } else if state.complete {
            let evidence =
                state
                    .verification_progress
                    .map_or_else(String::new, |(verified, total, tier)| {
                        format!("    verified {verified}/{total} Host receipts ({tier})")
                    });
            (
                format!("Run complete{evidence}    q to return"),
                state.palette.success(),
            )
        } else if let Some((verified, total, tier)) = state.verification_progress {
            (
                format!("Verified {verified}/{total} Host receipts ({tier})"),
                state.palette.success(),
            )
        } else {
            ("Run in progress".to_owned(), state.palette.muted())
        };
        frame.render_widget(
            Paragraph::new(content)
                .style(style)
                .wrap(Wrap { trim: false }),
            status,
        );
        frame.render_widget(
            Paragraph::new(passive_hints(state, area.width)).style(state.palette.muted()),
            footer,
        );
    }
}

fn callout_detail_lines(state: &ScreenState, callout: &PendingCallout) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(callout.name.clone(), state.palette.emphasis()),
        Span::raw("  "),
        Span::raw(callout.prompt.clone()),
    ])];
    lines.push(Line::styled(
        format!(
            "Host {}  exec {}  pending {}  callout #{}",
            callout.host,
            callout.exec_id.fmt_short(),
            callout.pending_id,
            callout.callout_index
        ),
        state.palette.muted(),
    ));
    if let Some(options) = callout_options(&callout.schema) {
        lines.push(Line::from(vec![
            Span::styled("Options  ", state.palette.muted()),
            Span::styled(options, state.palette.strong()),
        ]));
    }
    if !callout.context.is_null() {
        lines.push(Line::from(vec![
            Span::styled("Context  ", state.palette.muted()),
            Span::raw(crate::ui::compact_json(&callout.context)),
        ]));
    }
    if let Some(error) = &callout.validation_error {
        lines.push(Line::styled(
            format!("Invalid answer  {error}"),
            state.palette.error(),
        ));
    }
    lines
}

fn composer_height(state: &ScreenState, width: u16) -> u16 {
    if state.callouts.is_empty() {
        if let Some(failure) = &state.failure {
            let content = Paragraph::new(format!("Failed    {failure}")).wrap(Wrap { trim: false });
            return u16::try_from(content.line_count(width).saturating_add(1))
                .unwrap_or(5)
                .clamp(2, 5);
        }
        return 2;
    }
    let callout = state.callouts.selected().expect("checked callout presence");
    let inner_width = width.saturating_sub(2).max(1);
    let details = Paragraph::new(callout_detail_lines(state, callout))
        .wrap(Wrap { trim: false })
        .line_count(inner_width);
    let input = callout.editor.lines().len().max(1);
    u16::try_from(details.saturating_add(input).saturating_add(3)).unwrap_or(u16::MAX)
}

fn composer_overflows(state: &ScreenState, area: Rect) -> bool {
    composer_height(state, area.width) > area.height
}

fn system_event_line(frame: &EventFrame, state: &ScreenState) -> Line<'static> {
    let detail = events::event_summary(frame);
    let kind = frame.kind().strip_prefix("exec.").unwrap_or(frame.kind());
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!("    {detail}")
    };
    Line::from(vec![
        Span::styled(
            format!("{}  ", events::event_time(frame.ts)),
            state.palette.muted(),
        ),
        Span::styled(format!("{:<10}", frame.host), state.palette.strong()),
        Span::styled(format!("#{:04}  ", frame.seq), state.palette.muted()),
        Span::styled(kind.to_owned(), state.palette.emphasis()),
        Span::raw(detail),
    ])
}

fn lifecycle_line(state: &ScreenState, compact: bool) -> Line<'static> {
    let scoped = state
        .scoped_host_names()
        .into_iter()
        .filter_map(|host| state.host_status(&host).map(|status| (host, status)))
        .collect::<Vec<_>>();
    if state.scoped_run_state() == ScopedRunState::Mixed {
        let details = scoped
            .iter()
            .map(|(host, status)| {
                format!(
                    "{} {} #{}",
                    host,
                    format!("{:?}", status.lifecycle()).to_lowercase(),
                    status
                        .step()
                        .map_or_else(|| "-".to_owned(), |step| step.to_string())
                )
            })
            .collect::<Vec<_>>()
            .join("    ");
        return Line::from(vec![
            Span::styled(
                "STATUS MIXED    ",
                state.palette.error().add_modifier(Modifier::BOLD),
            ),
            Span::raw(details),
        ]);
    }
    if let ScopedRunState::Waiting { observed, expected } = state.scoped_run_state() {
        return Line::from(vec![
            Span::styled("STATUS WAITING    ", state.palette.strong()),
            Span::raw(format!("Hosts {observed}/{expected} observed")),
        ]);
    }
    let session = state
        .session_id()
        .map(|id| id.fmt_short().to_string())
        .unwrap_or_else(|| "pending".to_owned());
    let step = match state.scoped_run_state() {
        ScopedRunState::Uniform { step, .. } => step,
        ScopedRunState::Waiting { .. } | ScopedRunState::Mixed => None,
    }
    .map_or_else(|| "-".to_owned(), |step| step.to_string());
    let agreement = state.scoped_agreement_label();
    let label = state.scoped_status_label();
    if compact {
        return Line::from(vec![
            Span::styled("STATUS ", lifecycle_style(state)),
            Span::styled(label, lifecycle_style(state)),
            Span::styled("    ", state.palette.muted()),
            Span::raw(session),
            Span::styled("    #", state.palette.muted()),
            Span::raw(step),
            Span::styled("    ", state.palette.muted()),
            Span::raw(agreement),
        ]);
    }
    let expected_hosts = state.scoped_host_names().len();
    let host_progress = if expected_hosts > 1 {
        format!("    Hosts {}/{}", scoped.len(), expected_hosts)
    } else {
        String::new()
    };
    Line::from(vec![
        Span::styled("STATUS ", lifecycle_style(state)),
        Span::styled(label, lifecycle_style(state)),
        Span::styled("    session ", state.palette.muted()),
        Span::raw(session),
        Span::styled("    step ", state.palette.muted()),
        Span::raw(step),
        Span::styled("    agreement ", state.palette.muted()),
        Span::raw(agreement),
        Span::styled(host_progress, state.palette.muted()),
    ])
}

fn view_history_label(state: &ScreenState) -> String {
    let view_count = state.view_count();
    if view_count == 0 {
        return "WAITING".to_owned();
    }
    if state.is_live_view() {
        "LIVE".to_owned()
    } else if state.view_new_count == 0 {
        format!("HISTORY {}/{}", state.view_cursor + 1, view_count)
    } else {
        format!(
            "HISTORY {}/{}  +{} new",
            state.view_cursor + 1,
            view_count,
            state.view_new_count
        )
    }
}

fn panel(title: impl Into<String>, state: &ScreenState, focused: bool) -> Block<'static> {
    let style = if focused {
        state.palette.strong()
    } else {
        state.palette.muted()
    };
    let title_style = if focused {
        style.add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        style
    };
    let title = Line::from(vec![
        Span::styled(if focused { " ▸ " } else { "  " }, title_style),
        Span::styled(title.into(), title_style),
        Span::styled(" ", title_style),
        Span::raw(" "),
    ]);
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .merge_borders(MergeStrategy::Exact)
        .border_style(style)
}

fn overview_panel(
    title: impl Into<String>,
    state: &ScreenState,
    area: Rect,
    focused: bool,
) -> Block<'static> {
    let mut title = title.into();
    if focused
        && crate::ui::display_width(&title).saturating_add("  ENTER OPEN".len())
            <= usize::from(area.width.saturating_sub(6))
    {
        title.push_str("  ENTER OPEN");
    }
    panel(title, state, focused)
}

fn callout_options(schema: &Value) -> Option<String> {
    let values = schema.get("enum")?.as_array()?;
    (!values.is_empty()).then(|| {
        values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join("    ")
    })
}

fn key_hints(state: &ScreenState, width: u16, overflow: bool) -> &'static str {
    if overflow && width >= 92 {
        "Enter submit    Ctrl-PgUp/PgDn details    Tab focus    Ctrl-C stop"
    } else if state.callouts.len() > 1 && width >= 100 {
        "Enter submit    [ ] request    Tab focus    Esc overview    ? help    Ctrl-C stop"
    } else if width >= 84 {
        "Enter submit    Tab focus    Esc overview    ? help    Ctrl-C stop"
    } else {
        "Enter submit    Tab focus    ? help    Ctrl-C stop"
    }
}

fn passive_hints(state: &ScreenState, width: u16) -> String {
    let answer = if !state.callouts.is_empty() {
        "i input    "
    } else {
        ""
    };
    let navigation = if state.page.view() == WorkspaceView::Overview {
        if state.complete {
            "1–6 views    Tab focus    Enter select/open    q return    ? help"
        } else if width >= 72 {
            "1–6 views    Tab focus    Enter select/open    ↑↓ move    ? help"
        } else {
            "1–6 views    Tab focus    Enter open    ? help"
        }
    } else if state.complete {
        "1–6 views    Enter inspect    Esc overview    q return    ? help"
    } else if state.page.view() == WorkspaceView::Wasm && width >= 86 {
        "1–6 views    Tab focus    < older    > newer    Esc overview"
    } else if width >= 86 {
        "1–6 views    Tab focus    Enter select/inspect    Esc overview    c compare"
    } else {
        "1–6 views    Enter inspect    Esc overview    ? help"
    };
    format!("{answer}{navigation}")
}

fn lifecycle_style(state: &ScreenState) -> Style {
    match state.lifecycle() {
        ExecLifecycle::Completed => state.palette.success(),
        ExecLifecycle::Aborted | ExecLifecycle::Incomplete | ExecLifecycle::Failed => {
            state.palette.error()
        }
        ExecLifecycle::Negotiating
        | ExecLifecycle::Activating
        | ExecLifecycle::Waiting
        | ExecLifecycle::Active => state.palette.strong(),
    }
}

fn plain_slot(value: &str) -> String {
    sanitize::sanitize(value)
        .lines()
        .map(sanitize::strip_ansi)
        .collect::<Vec<_>>()
        .join("\n")
}

fn program_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).max(1)
}

fn program_view_width(state: &ScreenState, area: Rect) -> u16 {
    let layout = screen_layout(area, composer_height(state, canvas_width(area.width)));
    let projection = if state.page.view() == WorkspaceView::Overview {
        overview_layout(state, layout.workspace).program
    } else {
        layout.workspace
    };
    match (&state.host_scope, state.page.view()) {
        (HostScope::Compare { .. }, WorkspaceView::Program) => {
            projection.width.saturating_sub(5).saturating_div(2).max(1)
        }
        _ => program_width(projection),
    }
}

fn publish_width(width: &watch::Sender<u16>, next: u16) {
    if *width.borrow() != next {
        width.send_replace(next);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests;
