//! Interactive catalog and launch setup for one local coordinated run.

use crate::ui::TuiPalette;
use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::{Context as _, anyhow, bail};
use arena0_client::api::{ExecStatus, HostRequest, ProgramDetail, ResponseOk};
use arena0_client::proto::DaemonClient;
use arena0_client::protocol::ProgramHash;
use arena0_home::HostName;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt as _;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::{Marker, border};
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use serde_json::Value;
use unicode_width::UnicodeWidthChar as _;

const MIN_WIDTH: u16 = 48;
const MIN_HEIGHT: u16 = 23;
const WIDE_WIDTH: u16 = 96;
const SPLASH_LOGO_WIDTH: u16 = 40;
const LOGO_SEGMENTS: [[f64; 4]; 29] = [
    [1.0, 1.0, 7.0, 19.0],
    [7.0, 19.0, 13.0, 1.0],
    [3.5, 8.0, 10.5, 8.0],
    [18.0, 1.0, 18.0, 19.0],
    [18.0, 19.0, 28.0, 19.0],
    [28.0, 19.0, 31.0, 16.0],
    [31.0, 16.0, 31.0, 12.0],
    [31.0, 12.0, 28.0, 9.0],
    [28.0, 9.0, 18.0, 9.0],
    [26.0, 9.0, 32.0, 1.0],
    [37.0, 1.0, 37.0, 19.0],
    [37.0, 19.0, 50.0, 19.0],
    [37.0, 10.0, 48.0, 10.0],
    [37.0, 1.0, 50.0, 1.0],
    [56.0, 1.0, 56.0, 19.0],
    [56.0, 19.0, 69.0, 1.0],
    [69.0, 1.0, 69.0, 19.0],
    [75.0, 1.0, 81.0, 19.0],
    [81.0, 19.0, 87.0, 1.0],
    [77.5, 8.0, 84.5, 8.0],
    [94.0, 4.0, 94.0, 16.0],
    [94.0, 16.0, 97.0, 19.0],
    [97.0, 19.0, 103.0, 19.0],
    [103.0, 19.0, 106.0, 16.0],
    [106.0, 16.0, 106.0, 4.0],
    [106.0, 4.0, 103.0, 1.0],
    [103.0, 1.0, 97.0, 1.0],
    [97.0, 1.0, 94.0, 4.0],
    [96.0, 3.0, 104.0, 17.0],
];

/// The exact local setup selected by the user.
#[derive(Debug)]
pub(crate) struct Launch {
    pub(crate) program: String,
    pub(crate) hosts: Vec<HostName>,
    pub(crate) input_control: InputControl,
    pub(crate) params: Option<Value>,
    pub(crate) replay: bool,
}

/// Initial values supplied by the existing launch command or bare workspace.
#[derive(Debug)]
pub(crate) struct Setup {
    pub(crate) hosts: Vec<HostName>,
    pub(crate) fixed_hosts: bool,
    pub(crate) bindings: Option<Vec<crate::coordinated::DriverBinding>>,
    pub(crate) params: Option<Value>,
    pub(crate) replay: bool,
}

/// Which local Hosts send their callouts to the shared human interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputControl {
    OneHost { host: HostName },
    AllHosts,
    Configured(Vec<crate::coordinated::DriverBinding>),
}

#[derive(Debug)]
pub(crate) enum Exit {
    Launch(Launch),
    Agents(String),
    Quit,
}

/// Load the typed catalog and execution summary used by the workspace.
pub(crate) async fn load(
    client: &DaemonClient,
    host: &HostName,
) -> anyhow::Result<(Vec<ProgramDetail>, Vec<ExecStatus>)> {
    let programs = match client.call_host(host, &HostRequest::ProgramList).await? {
        ResponseOk::ProgramList(programs) => programs,
        other => bail!("unexpected program.list response: {other:?}"),
    };
    let mut details = Vec::with_capacity(programs.len());
    for program in programs {
        let response = client
            .call_host(
                host,
                &HostRequest::ProgramGet {
                    program: program.program_hash.to_string(),
                },
            )
            .await
            .with_context(|| format!("load program '{}' for workspace", program.name))?;
        let ResponseOk::Program(detail) = response else {
            bail!("unexpected program.get response: {response:?}");
        };
        details.push(*detail);
    }
    details.sort_by(|left, right| left.summary.display_name.cmp(&right.summary.display_name));
    let executions = match client.call_host(host, &HostRequest::ExecList).await? {
        ResponseOk::ExecList(executions) => executions,
        other => bail!("unexpected exec.list response: {other:?}"),
    };
    Ok((details, executions))
}

/// Present the catalog and return one validated launch selection.
pub(crate) async fn choose(
    programs: Vec<ProgramDetail>,
    executions: Vec<ExecStatus>,
    setup: Setup,
) -> anyhow::Result<Exit> {
    if programs.is_empty() {
        bail!("the local program catalog is empty");
    }
    let mut state = State::new(programs, executions, setup.hosts.len(), !setup.fixed_hosts);
    state.configure(setup);
    choose_state(state).await
}

/// Select one two-Participant program and the harness that will run both agents.
pub(crate) async fn choose_agents(
    programs: Vec<ProgramDetail>,
    executions: Vec<ExecStatus>,
) -> anyhow::Result<Exit> {
    if programs.is_empty() {
        bail!("the local program catalog is empty");
    }
    let mut state = State::new(programs, executions, 2, false);
    state.agent_launch = true;
    choose_state(state).await
}

async fn choose_state(mut state: State) -> anyhow::Result<Exit> {
    let (mut terminal, _restore) = crate::terminal::enter()?;
    let mut events = EventStream::new();
    loop {
        let area = terminal.size().context("read workspace terminal size")?;
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            return Err(anyhow!(
                "terminal is too small for the workspace (minimum {MIN_WIDTH}x{MIN_HEIGHT})"
            ));
        }
        terminal
            .draw(|frame| render(frame, &state))
            .context("draw arena0 workspace")?;
        let event = events
            .next()
            .await
            .ok_or_else(|| anyhow!("terminal event stream closed"))??;
        if let Event::Key(key) = event
            && let Some(exit) = state.on_key(key)
        {
            return exit;
        }
    }
}

#[derive(Debug)]
enum Mode {
    Browse,
    EditParams { draft: String },
    Help,
}

#[derive(Debug)]
struct State {
    programs: Vec<ProgramDetail>,
    executions: Vec<ExecStatus>,
    selected: usize,
    setup_focused: bool,
    setup_scroll: u16,
    setup_scroll_limit: std::cell::Cell<u16>,
    participants: usize,
    input_control: InputControl,
    replay: bool,
    params: HashMap<ProgramHash, String>,
    hosts: Vec<HostName>,
    can_resize_hosts: bool,
    agent_launch: bool,
    mode: Mode,
    error: Option<String>,
    palette: TuiPalette,
}

impl State {
    fn new(
        programs: Vec<ProgramDetail>,
        executions: Vec<ExecStatus>,
        available_hosts: usize,
        can_resize_hosts: bool,
    ) -> Self {
        let participants = initial_participants(&programs[0], available_hosts, can_resize_hosts);
        Self {
            programs,
            executions,
            selected: 0,
            setup_focused: false,
            setup_scroll: 0,
            setup_scroll_limit: std::cell::Cell::new(0),
            participants,
            input_control: InputControl::OneHost {
                host: HostName::for_local_index(0),
            },
            replay: true,
            params: HashMap::new(),
            hosts: crate::local_daemon::host_names(available_hosts),
            can_resize_hosts,
            agent_launch: false,
            mode: Mode::Browse,
            error: None,
            palette: TuiPalette::detect(),
        }
    }

    fn configure(&mut self, setup: Setup) {
        self.hosts = setup.hosts;
        self.can_resize_hosts = !setup.fixed_hosts;
        self.replay = setup.replay;
        if let Some(params) = setup.params {
            let text = params.to_string();
            for program in &self.programs {
                self.params
                    .insert(program.summary.program_hash, text.clone());
            }
        }
        self.input_control = setup.bindings.map_or_else(
            || InputControl::OneHost {
                host: self.host_at(0),
            },
            InputControl::Configured,
        );
        self.program_changed();
    }

    fn host_at(&self, index: usize) -> HostName {
        self.hosts
            .get(index)
            .cloned()
            .unwrap_or_else(|| HostName::for_local_index(index))
    }

    fn program(&self) -> &ProgramDetail {
        &self.programs[self.selected]
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<anyhow::Result<Exit>> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Ok(Exit::Quit));
        }
        self.setup_scroll = self.setup_scroll.min(self.setup_scroll_limit.get());
        match &mut self.mode {
            Mode::EditParams { draft } => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.error = None;
                    None
                }
                KeyCode::Enter => {
                    let draft = draft.clone();
                    let validation = validate_params(self.program(), &draft);
                    match validation {
                        Ok(_) => {
                            self.params
                                .insert(self.program().summary.program_hash, draft);
                            self.mode = Mode::Browse;
                            self.error = None;
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                    None
                }
                KeyCode::Backspace => {
                    draft.pop();
                    None
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    draft.push(character);
                    None
                }
                _ => None,
            },
            Mode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.mode = Mode::Browse;
                }
                None
            }
            Mode::Browse => {
                if self.agent_launch
                    && matches!(
                        key.code,
                        KeyCode::Char('+' | '=' | '-' | 'h' | 'c' | 'r' | 'p')
                    )
                {
                    return None;
                }
                match key.code {
                    KeyCode::Tab | KeyCode::BackTab => {
                        self.setup_focused = !self.setup_focused;
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') if self.setup_focused => {
                        self.setup_scroll = self.setup_scroll.saturating_sub(1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') if self.setup_focused => {
                        self.setup_scroll = self.setup_scroll.saturating_add(1);
                        None
                    }
                    KeyCode::Char('u' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if self.setup_focused {
                            self.setup_scroll = if key.code == KeyCode::Char('u') {
                                self.setup_scroll.saturating_sub(8)
                            } else {
                                self.setup_scroll.saturating_add(8)
                            };
                        } else {
                            self.selected = if key.code == KeyCode::Char('u') {
                                self.selected.saturating_sub(8)
                            } else {
                                self.selected
                                    .saturating_add(8)
                                    .min(self.programs.len().saturating_sub(1))
                            };
                            self.program_changed();
                        }
                        None
                    }
                    KeyCode::Char('g' | 'G') => {
                        if self.setup_focused {
                            self.setup_scroll = if key.code == KeyCode::Char('g') {
                                0
                            } else {
                                u16::MAX
                            };
                        } else {
                            self.selected = if key.code == KeyCode::Char('g') {
                                0
                            } else {
                                self.programs.len().saturating_sub(1)
                            };
                            self.program_changed();
                        }
                        None
                    }
                    KeyCode::Char('q') => Some(Ok(Exit::Quit)),
                    KeyCode::Char('?') => {
                        self.mode = Mode::Help;
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') if !self.setup_focused => {
                        if self.selected > 0 {
                            self.selected -= 1;
                            self.program_changed();
                        }
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') if !self.setup_focused => {
                        if self.selected + 1 < self.programs.len() {
                            self.selected += 1;
                            self.program_changed();
                        }
                        None
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        self.adjust_participants(1);
                        None
                    }
                    KeyCode::Char('-') => {
                        self.adjust_participants(-1);
                        None
                    }
                    KeyCode::Char('h') => {
                        self.select_next_human_host();
                        self.error = None;
                        None
                    }
                    KeyCode::Char('c')
                        if !matches!(self.input_control, InputControl::Configured(_)) =>
                    {
                        self.input_control = match &self.input_control {
                            InputControl::OneHost { .. } => InputControl::AllHosts,
                            InputControl::AllHosts => InputControl::OneHost {
                                host: self.host_at(0),
                            },
                            InputControl::Configured(_) => {
                                unreachable!("configured bindings are fixed")
                            }
                        };
                        self.error = None;
                        None
                    }
                    KeyCode::Char('r') => {
                        self.replay = !self.replay;
                        self.error = None;
                        None
                    }
                    KeyCode::Char('p') => {
                        let draft = self.params_text().into_owned();
                        self.mode = Mode::EditParams { draft };
                        self.error = None;
                        None
                    }
                    KeyCode::Enter => match self.selection() {
                        Ok(launch) => Some(Ok(launch)),
                        Err(error) => {
                            if !self.agent_launch {
                                let draft = self.params_text().into_owned();
                                self.mode = Mode::EditParams { draft };
                            }
                            self.error = Some(error.to_string());
                            None
                        }
                    },
                    _ => None,
                }
            }
        }
    }

    fn program_changed(&mut self) {
        if self.agent_launch {
            self.participants = 2;
            self.error = None;
            return;
        }
        self.participants =
            initial_participants(self.program(), self.hosts.len(), self.can_resize_hosts);
        self.clamp_human_host();
        self.error = None;
    }

    fn adjust_participants(&mut self, delta: i8) {
        if !self.can_resize_hosts {
            self.error = Some(format!(
                "the command selected {} Hosts; edit --hosts or driver bindings to change them",
                self.hosts.len()
            ));
            return;
        }
        let (min, max) = self.program().summary.participants.bounds();
        let next = if delta > 0 {
            self.participants.saturating_add(1)
        } else {
            self.participants.saturating_sub(1)
        };
        self.participants = next.clamp(usize::from(min), usize::from(max));
        self.clamp_human_host();
        self.error = None;
    }

    fn select_next_human_host(&mut self) {
        let InputControl::OneHost { host } = &self.input_control else {
            return;
        };
        let current = (0..self.participants)
            .position(|index| self.host_at(index) == *host)
            .unwrap_or(0);
        self.input_control = InputControl::OneHost {
            host: self.host_at((current + 1) % self.participants.max(1)),
        };
    }

    fn clamp_human_host(&mut self) {
        let InputControl::OneHost { host } = &self.input_control else {
            return;
        };
        if !(0..self.participants).any(|index| self.host_at(index) == *host) {
            self.input_control = InputControl::OneHost {
                host: self.host_at(self.participants.saturating_sub(1)),
            };
        }
    }

    fn params_text(&self) -> Cow<'_, str> {
        self.params
            .get(&self.program().summary.program_hash)
            .map_or_else(
                || Cow::Owned(default_params(self.program(), self.participants)),
                |params| Cow::Borrowed(params.as_str()),
            )
    }

    fn launch(&mut self) -> anyhow::Result<Launch> {
        let program = self.program();
        let participants = u16::try_from(self.participants).context("too many local Hosts")?;
        if !program.summary.participants.accepts(participants) {
            bail!(
                "{} requires {} participants; the current service provides {} Hosts",
                program.summary.display_name,
                program.summary.participants,
                self.participants
            );
        }
        let params_text = self.params_text();
        let params = validate_params(program, params_text.as_ref())?;
        Ok(Launch {
            program: program.summary.program_hash.to_string(),
            hosts: (0..self.participants)
                .map(|index| self.host_at(index))
                .collect(),
            input_control: self.input_control.clone(),
            params,
            replay: self.replay,
        })
    }

    fn selection(&mut self) -> anyhow::Result<Exit> {
        if !self.agent_launch {
            return self.launch().map(Exit::Launch);
        }
        let program = self.program();
        if !program.summary.participants.accepts(2) {
            bail!(
                "{} does not support the two Participants required by agent launch",
                program.summary.display_name
            );
        }
        Ok(Exit::Agents(program.summary.program_hash.to_string()))
    }
}

fn render(frame: &mut Frame<'_>, state: &State) {
    let area = frame.area();
    let footer_height = if area.width >= 104 { 3 } else { 4 };
    let show_error = state.error.is_some() && !matches!(state.mode, Mode::EditParams { .. });
    let (main, error, footer) = if show_error {
        let [main, error, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(10),
                Constraint::Length(3),
                Constraint::Length(footer_height),
            ])
            .areas(area);
        (main, error, footer)
    } else {
        let [main, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(footer_height)])
            .areas(area);
        (main, Rect::default(), footer)
    };
    if area.width >= WIDE_WIDTH {
        let [left, setup] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
            .areas(main);
        let [splash, catalog] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(splash_height(left.width)),
                Constraint::Min(7),
            ])
            .areas(left);
        render_splash(frame, state, splash);
        render_catalog(frame, state, catalog);
        render_setup(frame, state, setup);
    } else {
        let catalog_height = u16::try_from(state.programs.len().saturating_add(2))
            .unwrap_or(10)
            .min(10);
        let [splash, catalog, setup] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(splash_height(main.width)),
                Constraint::Length(catalog_height),
                Constraint::Min(8),
            ])
            .areas(main);
        render_splash(frame, state, splash);
        render_catalog(frame, state, catalog);
        render_setup(frame, state, setup);
    }
    if show_error && let Some(message) = &state.error {
        frame.render_widget(
            Paragraph::new(message.as_str())
                .style(state.palette.error())
                .block(panel(" ERROR ", state.palette.error(), state)),
            error,
        );
    }
    let keys = if state.agent_launch {
        vec![Line::raw(
            "↑↓ program    Tab pane    Enter launch    ? help    q quit",
        )]
    } else if matches!(state.input_control, InputControl::Configured(_)) {
        vec![Line::raw(
            "↑↓ move  Tab pane    p params    r replay    Enter launch    ? help    q quit",
        )]
    } else if footer.width >= 104 {
        vec![Line::raw(
            "↑↓ move  Tab pane    +/- Hosts    c control    h Host    p params    r replay    Enter run    ? help    q quit",
        )]
    } else {
        vec![
            Line::raw("↑↓ move  Tab pane    +/- Hosts    c control    h Host"),
            Line::raw("p params    r replay    Enter run    ? help    q quit"),
        ]
    };
    frame.render_widget(
        Paragraph::new(keys)
            .style(state.palette.muted())
            .block(panel(" COMMANDS ", state.palette.muted(), state)),
        footer,
    );
    match &state.mode {
        Mode::EditParams { draft } => render_params_editor(frame, state, draft),
        Mode::Help => render_help(frame, state),
        Mode::Browse => {}
    }
}

const fn splash_height(width: u16) -> u16 {
    if width >= 62 { 11 } else { 12 }
}

fn render_splash(frame: &mut Frame<'_>, state: &State, area: Rect) {
    let capability_height = if area.width >= 62 { 1 } else { 2 };
    let [logo_row, _, subtitle, _, capabilities, _] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(capability_height),
            Constraint::Length(2),
        ])
        .areas(area);
    let logo_width = SPLASH_LOGO_WIDTH.min(logo_row.width);
    let logo = Rect::new(
        logo_row
            .x
            .saturating_add(logo_row.width.saturating_sub(logo_width) / 2),
        logo_row.y,
        logo_width,
        logo_row.height,
    );
    let color = state.palette.strong().fg.unwrap_or(Color::Reset);
    frame.render_widget(
        Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, 108.0])
            .y_bounds([0.0, 20.0])
            .paint(move |context| {
                for [x1, y1, x2, y2] in LOGO_SEGMENTS {
                    context.draw(&CanvasLine::new(x1, y1, x2, y2, color));
                }
            }),
        logo,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "verifiable co-execution for agents",
            state.palette.emphasis(),
        ))
        .alignment(Alignment::Center),
        subtitle,
    );
    let mut lines = Vec::new();
    if area.width >= 62 {
        lines.push(Line::styled(
            "Wasm, p2p, state machines, cryptographic receipts",
            state.palette.public(),
        ));
    } else {
        lines.push(Line::styled(
            "Wasm, p2p, state machines,",
            state.palette.public(),
        ));
        lines.push(Line::styled(
            "cryptographic receipts",
            state.palette.public(),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        capabilities,
    );
}

fn render_catalog(frame: &mut Frame<'_>, state: &State, area: Rect) {
    let title = format!(
        " PROGRAMS  {} AVAILABLE{} ",
        state.programs.len(),
        if state.setup_focused { "" } else { "  [FOCUS]" }
    );
    let block = panel(
        &title,
        if state.setup_focused {
            state.palette.muted()
        } else {
            state.palette.strong()
        },
        state,
    );
    let body = block.inner(area);
    let lines = state
        .programs
        .iter()
        .enumerate()
        .map(|(index, detail)| {
            let selected = index == state.selected;
            let marker = if selected { "▸" } else { " " };
            let name_width = usize::from(body.width).saturating_sub(2);
            let name = truncate(&detail.summary.display_name, name_width);
            Line::from(vec![
                Span::styled(format!("{marker} "), selected_style(state, selected)),
                Span::styled(name, selected_style(state, selected)),
            ])
        })
        .collect::<Vec<_>>();
    let scroll = state
        .selected
        .saturating_sub(usize::from(body.height.saturating_sub(1)));
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
            .block(block),
        area,
    );
}

fn render_setup(frame: &mut Frame<'_>, state: &State, area: Rect) {
    if state.agent_launch {
        render_agent_setup(frame, state, area);
        return;
    }
    let program = state.program();
    let control_label = match state.input_control {
        InputControl::OneHost { .. } => "[●] One Host    [ ] All Hosts",
        InputControl::AllHosts => "[ ] One Host    [●] All Hosts",
        InputControl::Configured(_) => "Configured by command flags",
    };
    let mut lines = vec![
        Line::styled(
            format!(
                "{} Hosts ready    {} known executions",
                state.hosts.len(),
                state.executions.len()
            ),
            state.palette.muted(),
        ),
        Line::default(),
        Line::styled(
            program.summary.display_name.clone(),
            state.palette.emphasis(),
        ),
        Line::styled(program.summary.description.clone(), Style::default()),
        Line::default(),
        labeled(state, "Version", program.summary.version.clone()),
        labeled(
            state,
            "Program ID",
            program.summary.program_hash.fmt_short().to_string(),
        ),
        labeled(
            state,
            "Participants",
            program.summary.participants.to_string(),
        ),
        Line::default(),
        labeled(state, "Hosts", state.participants.to_string()),
        Line::default(),
        Line::styled("INPUT CONTROL", state.palette.emphasis()),
        Line::styled(control_label, state.palette.strong()),
        Line::default(),
        Line::from(vec![
            Span::styled(format!("{:<14}", "HOST"), state.palette.muted()),
            Span::styled("INPUT DRIVER", state.palette.muted()),
        ]),
    ];
    for index in 0..state.participants {
        let host = state.host_at(index);
        let driver = match &state.input_control {
            InputControl::AllHosts => "YOU ANSWER".to_owned(),
            InputControl::OneHost { host: human } if *human == host => "YOU ANSWER".to_owned(),
            InputControl::OneHost { .. } => "BUILTIN sample".to_owned(),
            InputControl::Configured(bindings) => bindings
                .iter()
                .find(|binding| binding.host == host)
                .map_or_else(
                    || "EXTERNAL CLIENT".to_owned(),
                    |binding| match &binding.driver {
                        crate::coordinated::DriverSpec::External => "EXTERNAL CLIENT".to_owned(),
                        crate::coordinated::DriverSpec::Human => "YOU ANSWER".to_owned(),
                        crate::coordinated::DriverSpec::Builtin(strategy) => {
                            format!("BUILTIN {strategy}")
                        }
                        crate::coordinated::DriverSpec::Executable(path) => {
                            format!("AGENT {}", path.display())
                        }
                    },
                ),
        };
        lines.push(labeled(state, &format!("  {host}"), driver.to_owned()));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        match &state.input_control {
            InputControl::OneHost { host } => {
                format!("You answer {host}. Other Hosts use the sample policy.")
            }
            InputControl::AllHosts => {
                "You answer each Host independently. Concurrent requests are queued.".to_owned()
            }
            InputControl::Configured(_) => {
                "Unbound Hosts wait for an external client or monitor answer.".to_owned()
            }
        },
        state.palette.muted(),
    ));
    lines.extend([
        labeled(state, "Parameters", state.params_text().into_owned()),
        labeled(
            state,
            "Verification",
            if state.replay { "full replay" } else { "light" }.to_owned(),
        ),
    ]);
    if !state.executions.is_empty() {
        lines.push(Line::default());
        lines.push(Line::styled("Recent executions", state.palette.emphasis()));
        for execution in state.executions.iter().rev().take(3) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}  ", execution.exec_id.fmt_short()),
                    state.palette.muted(),
                ),
                Span::raw(format!("{:?}", execution.lifecycle()).to_lowercase()),
            ]));
        }
    }
    let title = if state.setup_focused {
        " RUN SETUP  [FOCUS] "
    } else {
        " RUN SETUP "
    };
    render_setup_panel(frame, state, area, title, lines);
}

fn render_agent_setup(frame: &mut Frame<'_>, state: &State, area: Rect) {
    let program = state.program();
    let lines = vec![
        Line::styled(
            program.summary.display_name.clone(),
            state.palette.emphasis(),
        ),
        Line::styled(program.summary.description.clone(), Style::default()),
        Line::default(),
        labeled(state, "Version", program.summary.version.clone()),
        labeled(
            state,
            "Program ID",
            program.summary.program_hash.fmt_short().to_string(),
        ),
        labeled(
            state,
            "Participants",
            program.summary.participants.to_string(),
        ),
        Line::default(),
        Line::styled("HARNESS", state.palette.emphasis()),
        Line::styled("[●] Codex", state.palette.strong()),
        Line::default(),
        Line::styled("SESSIONS", state.palette.emphasis()),
        labeled(state, "Current pane", "Codex".to_owned()),
        labeled(state, "New right pane", "Codex".to_owned()),
        Line::default(),
        Line::styled(
            "Two agents discover each other through the program topic.",
            state.palette.muted(),
        ),
        Line::styled(
            "Runtime state uses an isolated temporary home.",
            state.palette.muted(),
        ),
    ];
    let title = if state.setup_focused {
        " AGENT LAUNCH  [FOCUS] "
    } else {
        " AGENT LAUNCH "
    };
    render_setup_panel(frame, state, area, title, lines);
}

fn render_setup_panel(
    frame: &mut Frame<'_>,
    state: &State,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
) {
    let block = panel(
        title,
        if state.setup_focused {
            state.palette.strong()
        } else {
            state.palette.muted()
        },
        state,
    );
    let inner = block.inner(area);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let last = paragraph
        .line_count(inner.width)
        .saturating_sub(usize::from(inner.height));
    let last = u16::try_from(last).unwrap_or(u16::MAX);
    state.setup_scroll_limit.set(last);
    frame.render_widget(
        paragraph
            .scroll((state.setup_scroll.min(last), 0))
            .block(block),
        area,
    );
}

fn render_params_editor(frame: &mut Frame<'_>, state: &State, draft: &str) {
    let area = crate::ui::centered(
        frame.area(),
        82,
        if state.error.is_some() { 16 } else { 15 },
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .title(Span::styled(" PARAMETERS  JSON ", state.palette.strong()))
        .border_style(state.palette.strong());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [schema, input, error, hint] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(u16::from(state.error.is_some())),
            Constraint::Length(1),
        ])
        .areas(inner);
    frame.render_widget(
        Paragraph::new(crate::ui::compact_json(
            state.program().schema.params.as_value(),
        ))
        .style(state.palette.muted())
        .wrap(Wrap { trim: false }),
        schema,
    );
    let available = input.width.saturating_sub(2);
    let width = u16::try_from(crate::ui::display_width(draft)).unwrap_or(u16::MAX);
    let horizontal = width.saturating_sub(available);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", state.palette.strong()),
            Span::raw(draft.to_owned()),
        ]))
        .scroll((0, horizontal)),
        input,
    );
    let cursor_x = input
        .x
        .saturating_add(2)
        .saturating_add(width.saturating_sub(horizontal))
        .min(input.x.saturating_add(input.width.saturating_sub(1)));
    frame.set_cursor_position((cursor_x, input.y));
    if let Some(message) = &state.error {
        frame.render_widget(
            Paragraph::new(message.as_str()).style(state.palette.error()),
            error,
        );
    }
    frame.render_widget(
        Paragraph::new("Enter apply    Esc cancel").style(state.palette.muted()),
        hint,
    );
}

fn render_help(frame: &mut Frame<'_>, state: &State) {
    let entries: &[(&str, &str)] = if state.agent_launch {
        &[
            (
                "Tab / Shift-Tab",
                "Move focus between Programs and Agent Launch",
            ),
            ("↑↓ / j/k", "Select a program or scroll Agent Launch"),
            ("Ctrl-U / Ctrl-D", "Jump up or down in the focused pane"),
            ("g / G", "First / last position"),
            ("Enter", "Launch two Codex Participants"),
            ("q / Ctrl-C", "Quit and stop the isolated local service"),
            ("? / Esc", "Close help"),
        ]
    } else {
        &[
            (
                "Tab / Shift-Tab",
                "Move focus between Programs and Run Setup",
            ),
            ("↑↓ / j/k", "Select a program or scroll Run Setup"),
            ("Ctrl-U / Ctrl-D", "Jump up or down in the focused pane"),
            ("g / G", "First / last position"),
            ("+/-", "Change the exact local Host count"),
            ("c", "Toggle One Host or All Hosts human control"),
            ("h", "Select the human-controlled Host in One Host mode"),
            ("p", "Edit program parameters as JSON"),
            ("r", "Toggle light verification or full replay"),
            ("Enter", "Launch the selected program"),
            ("q / Ctrl-C", "Quit and stop an owned local service"),
            ("? / Esc", "Close help"),
        ]
    };
    let height = u16::try_from(entries.len().saturating_add(2)).unwrap_or(u16::MAX);
    let area = crate::ui::centered(frame.area(), 76, height);
    frame.render_widget(Clear, area);
    let lines = entries
        .iter()
        .copied()
        .map(|(key, description)| {
            Line::from(vec![
                Span::styled(format!("{key:<18}"), state.palette.strong()),
                Span::raw(description),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .title(Span::styled(" HELP ", state.palette.strong()))
                .border_style(state.palette.strong()),
        ),
        area,
    );
}

fn labeled(state: &State, label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), state.palette.muted()),
        Span::raw(value),
    ])
}

fn panel<'a>(title: &'a str, title_style: Style, state: &State) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(state.palette.muted())
        .title(Span::styled(title, title_style))
}

fn selected_style(state: &State, selected: bool) -> Style {
    if !selected {
        Style::default()
    } else {
        state.palette.strong()
    }
}

fn initial_participants(
    program: &ProgramDetail,
    available_hosts: usize,
    can_resize_hosts: bool,
) -> usize {
    if can_resize_hosts {
        usize::from(program.summary.participants.bounds().0)
    } else {
        available_hosts
    }
}

fn default_params(program: &ProgramDetail, participants: usize) -> String {
    let schema = program.schema.params.as_value();
    match jsonschema::validator_for(schema) {
        Ok(validator) if validator.is_valid(&Value::Null) => return "null".to_owned(),
        _ => {}
    }
    let mut params = serde_json::Map::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            if let Some(default) = property.get("default") {
                params.insert(name.clone(), default.clone());
            }
        }
    }
    let target_size_is_required = schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| required.iter().any(|name| name == "target_size"));
    if target_size_is_required {
        params.insert("target_size".to_owned(), Value::from(participants));
    }
    Value::Object(params).to_string()
}

fn validate_params(program: &ProgramDetail, input: &str) -> anyhow::Result<Option<Value>> {
    let value: Value = serde_json::from_str(input).context("parameters must be valid JSON")?;
    let validator = jsonschema::validator_for(program.schema.params.as_value())
        .context("invalid program parameter schema")?;
    if let Err(error) = validator.validate(&value) {
        let path = error.instance_path().as_str();
        let path = if path.is_empty() { "$" } else { path };
        bail!("{path}: {error}");
    }
    Ok((value != Value::Null).then_some(value))
}

fn truncate(text: &str, max_width: usize) -> String {
    if crate::ui::display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width <= 1 {
        return "…".chars().take(max_width).collect();
    }
    let mut width = 0usize;
    let mut truncated = String::new();
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if width.saturating_add(character_width) >= max_width {
            break;
        }
        width += character_width;
        truncated.push(character);
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(participants: Value) -> ProgramDetail {
        program_with_params(
            participants,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "null"
            }),
        )
    }

    fn program_with_params(participants: Value, params: Value) -> ProgramDetail {
        let unit = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "null"
        });
        serde_json::from_value(serde_json::json!({
            "summary": {
                "program_hash": "0101010101010101010101010101010101010101010101010101010101010101",
                "name": "test",
                "display_name": "Test",
                "version": "1.0.0",
                "description": "Test program",
                "participants": participants
            },
            "schema": {
                "state": { "schema": unit, "max_bytes": 0 },
                "callouts": [],
                "messages": [],
                "params": params,
                "queries": [],
                "outcome": unit
            }
        }))
        .expect("program detail fixture")
    }

    #[test]
    fn launch_arrows_move_inside_the_pane_selected_by_tab() {
        let first = program(serde_json::json!({"kind":"exact","count":2}));
        let mut second = first.clone();
        second.summary.program_hash = ProgramHash([42; 32]);
        let selected_program = second.summary.program_hash.to_string();
        let mut state = State::new(vec![first, second], Vec::new(), 2, true);
        state.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.selected, 1);
        assert!(!state.setup_focused);
        state.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.selected, 0);
        state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(state.setup_focused);
        state.setup_scroll_limit.set(30);
        state.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(state.setup_scroll, 1);
        assert_eq!(state.selected, 0);
        for key in [KeyCode::Left, KeyCode::Right] {
            state.on_key(KeyEvent::new(key, KeyModifiers::NONE));
            assert!(state.setup_focused);
            assert_eq!(state.participants, 2);
        }
        state.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.setup_scroll, 0);
        state.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(!state.setup_focused);
        state.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let Some(Ok(Exit::Launch(launch))) =
            state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("selected program should launch");
        };
        assert_eq!(launch.program, selected_program);
    }

    #[test]
    fn launch_state_keeps_participant_bounds_and_clamps_one_human_host() {
        let mut state = State::new(
            vec![program(
                serde_json::json!({"kind": "range", "min": 2, "max": 4}),
            )],
            Vec::new(),
            2,
            true,
        );
        state.adjust_participants(1);
        state.adjust_participants(1);
        state.adjust_participants(1);
        assert_eq!(state.participants, 4);
        state.input_control = InputControl::OneHost {
            host: HostName::for_local_index(3),
        };
        state.adjust_participants(-1);
        assert_eq!(state.participants, 3);
        assert_eq!(
            state.input_control,
            InputControl::OneHost {
                host: HostName::for_local_index(2)
            }
        );
        assert!(state.launch().is_ok());
    }

    #[test]
    fn selection_preserves_exact_program_and_explicit_launch_inputs() {
        use crate::coordinated::{DriverBinding, DriverSpec};
        let first = program_with_params(
            serde_json::json!({"kind":"exact","count":2}),
            serde_json::json!({"type":"object","properties":{"rounds":{"type":"integer"}},"required":["rounds"]}),
        );
        let mut second = first.clone();
        second.summary.program_hash = ProgramHash([42; 32]);
        let selected = second.summary.program_hash;
        let hosts: Vec<HostName> = vec!["alice".parse().unwrap(), "bob".parse().unwrap()];
        let bindings = vec![
            DriverBinding::new(hosts[0].clone(), DriverSpec::Builtin("sample".into())),
            DriverBinding::new(hosts[1].clone(), DriverSpec::External),
        ];
        let mut state = State::new(vec![first, second], Vec::new(), 2, false);
        state.selected = 1;
        state.configure(Setup {
            hosts: hosts.clone(),
            fixed_hosts: true,
            bindings: Some(bindings.clone()),
            params: Some(serde_json::json!({"rounds":4})),
            replay: false,
        });
        let Some(Ok(Exit::Launch(launch))) =
            state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("launch selection");
        };
        assert_eq!(launch.program, selected.to_string());
        assert_eq!(launch.hosts, hosts);
        assert_eq!(launch.input_control, InputControl::Configured(bindings));
        assert_eq!(launch.params, Some(serde_json::json!({"rounds":4})));
        assert!(!launch.replay);
    }

    #[test]
    fn all_hosts_control_survives_participant_changes() {
        let mut state = State::new(
            vec![program(
                serde_json::json!({"kind": "range", "min": 2, "max": 4}),
            )],
            Vec::new(),
            2,
            true,
        );
        state.input_control = InputControl::AllHosts;

        state.adjust_participants(1);

        assert_eq!(state.input_control, InputControl::AllHosts);
        assert_eq!(
            state.launch().unwrap().input_control,
            InputControl::AllHosts
        );
    }

    #[test]
    fn invalid_launch_parameters_keep_the_workspace_open_for_correction() {
        let mut state = State::new(
            vec![program_with_params(
                serde_json::json!({"kind": "exact", "count": 2}),
                serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"item": {"type": "string"}},
                    "required": ["item"]
                }),
            )],
            Vec::new(),
            2,
            true,
        );

        let exit = state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(exit.is_none());
        assert!(matches!(state.mode, Mode::EditParams { .. }));
        assert!(
            state
                .error
                .as_deref()
                .is_some_and(|error| error.contains("item"))
        );

        state.params.insert(
            state.program().summary.program_hash,
            serde_json::json!({"item": "rare book"}).to_string(),
        );
        state.mode = Mode::Browse;
        let exit = state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(exit, Some(Ok(Exit::Launch(_)))));
    }

    #[test]
    fn default_parameters_follow_schema_defaults_and_selected_host_count() {
        let program = program_with_params(
            serde_json::json!({"kind": "range", "min": 2, "max": 4}),
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "target_size": {"type": "integer"},
                    "rounds": {"type": "integer", "default": 3}
                },
                "required": ["target_size", "rounds"]
            }),
        );

        assert_eq!(
            serde_json::from_str::<Value>(&default_params(&program, 4)).unwrap(),
            serde_json::json!({"target_size": 4, "rounds": 3})
        );
    }

    #[test]
    fn agent_launch_selects_a_two_participant_program_and_ignores_run_controls() {
        let selected = program(serde_json::json!({"kind":"exact","count":2}));
        let selected_id = selected.summary.program_hash.to_string();
        let mut state = State::new(vec![selected], Vec::new(), 2, false);
        state.agent_launch = true;

        let original_control = state.input_control.clone();
        for key in ['+', '-', 'c', 'h', 'p', 'r'] {
            assert!(
                state
                    .on_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                    .is_none()
            );
        }
        assert_eq!(state.participants, 2);
        assert_eq!(state.input_control, original_control);
        assert!(matches!(state.mode, Mode::Browse));
        assert!(state.replay);

        let Some(Ok(Exit::Agents(launch))) =
            state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("two-participant program should launch agents");
        };
        assert_eq!(launch, selected_id);
    }

    #[test]
    fn agent_launch_keeps_unsupported_programs_open_with_a_visible_error() {
        let mut state = State::new(
            vec![program(serde_json::json!({"kind":"exact","count":3}))],
            Vec::new(),
            2,
            false,
        );
        state.agent_launch = true;

        assert!(
            state
                .on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert!(matches!(state.mode, Mode::Browse));
        assert!(
            state
                .error
                .as_deref()
                .is_some_and(|message| message.contains("two Participants"))
        );
    }
}
