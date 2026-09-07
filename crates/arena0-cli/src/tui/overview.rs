//! Summary projections and responsive layout for the Overview page.

use super::*;

pub(super) fn render_overview(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let panes = overview_layout(state, area);
    let panes = [
        (OverviewPane::Negotiation, panes.negotiation),
        (OverviewPane::Program, panes.program),
        (OverviewPane::PublicTrace, panes.public_trace),
        (OverviewPane::Wasm, panes.wasm),
        (OverviewPane::SystemEvents, panes.system_events),
    ];
    for (pane, pane_area) in panes {
        if pane != state.page.pane() {
            render_overview_pane(frame, state, pane, pane_area, false);
        }
    }
    let focused_area = panes
        .into_iter()
        .find_map(|(pane, pane_area)| (pane == state.page.pane()).then_some(pane_area))
        .expect("overview focus always has a pane");
    render_overview_pane(
        frame,
        state,
        state.page.pane(),
        focused_area,
        state.focus == Focus::Workspace,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OverviewLayout {
    pub(super) negotiation: Rect,
    pub(super) program: Rect,
    pub(super) public_trace: Rect,
    pub(super) wasm: Rect,
    pub(super) system_events: Rect,
}

pub(super) fn overview_layout(state: &ScreenState, area: Rect) -> OverviewLayout {
    if area.width >= 100 && area.height >= 20 {
        let [left_width, _] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        let natural = overview_program_height(state, left_width.width);
        let top_height = natural.max(8).min(area.height.saturating_sub(12).max(8));
        let [top, middle, system_events] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(top_height),
                Constraint::Length(9),
                Constraint::Fill(1),
            ])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        let [program, negotiation] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .spacing(Spacing::Overlap(1))
            .areas(top);
        let [public_trace, wasm] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .spacing(Spacing::Overlap(1))
            .areas(middle);
        OverviewLayout {
            negotiation,
            program,
            public_trace,
            wasm,
            system_events,
        }
    } else if area.width >= 68 && area.height >= 14 {
        let natural = overview_program_height(state, area.width);
        let program_height = natural.max(6).min(area.height.saturating_sub(13).max(6));
        let [program, public_trace, pair, system_events] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(program_height),
                Constraint::Fill(2),
                Constraint::Length(4),
                Constraint::Length(4),
            ])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        let [negotiation, wasm] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .spacing(Spacing::Overlap(1))
            .areas(pair);
        OverviewLayout {
            negotiation,
            program,
            public_trace,
            wasm,
            system_events,
        }
    } else if area.height >= MIN_OVERVIEW_HEIGHT {
        let [program, public_trace, negotiation, wasm, system_events] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(3),
                Constraint::Fill(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        OverviewLayout {
            negotiation,
            program,
            public_trace,
            wasm,
            system_events,
        }
    } else {
        let [program, public_trace, negotiation, wasm, system_events] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
            ])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        OverviewLayout {
            negotiation,
            program,
            public_trace,
            wasm,
            system_events,
        }
    }
}

fn render_overview_pane(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    pane: OverviewPane,
    area: Rect,
    focused: bool,
) {
    match pane {
        OverviewPane::Negotiation => render_overview_negotiation(frame, state, area, focused),
        OverviewPane::Program => render_overview_program(frame, state, area, focused),
        OverviewPane::PublicTrace => render_overview_trace(frame, state, area, focused),
        OverviewPane::Wasm => render_overview_wasm(frame, state, area, focused),
        OverviewPane::SystemEvents => render_overview_events(frame, state, area, focused),
    }
}

fn render_overview_lines(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    area: Rect,
    focused: bool,
    title: impl Into<String>,
    lines: Vec<Line<'static>>,
) {
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overview_panel(title, state, area, focused)),
        area,
    );
}

fn render_overview_negotiation(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    area: Rect,
    focused: bool,
) {
    let mut lines = vec![Line::from(vec![
        Span::styled("stage  ", state.palette.muted()),
        Span::styled(state.scoped_status_label(), lifecycle_style(state)),
        Span::styled("    agreement  ", state.palette.muted()),
        Span::raw(state.scoped_agreement_label()),
    ])];
    let visible_hosts = state.scoped_host_names();
    let tickets = negotiation::latest_ticket_progress(state).map_or_else(
        || "pending".to_owned(),
        |(accepted, target)| format!("{accepted}/{target}"),
    );
    let activations = visible_hosts
        .iter()
        .filter(|host| {
            state
                .inspections
                .get(*host)
                .and_then(|inspection| inspection.activation.as_ref())
                .is_some()
        })
        .count();
    let sessions = visible_hosts
        .iter()
        .filter(|host| {
            state
                .host_status(host)
                .and_then(ExecStatus::session)
                .is_some()
        })
        .count();
    lines.push(Line::from(vec![
        Span::styled("tickets ", state.palette.muted()),
        Span::raw(tickets),
        Span::styled(" → activations ", state.palette.muted()),
        Span::raw(format!("{activations}/{}", visible_hosts.len())),
        Span::styled(" → sessions ", state.palette.muted()),
        Span::raw(format!("{sessions}/{}", visible_hosts.len())),
    ]));
    for (host, inspection) in state
        .inspections
        .iter()
        .filter(|(host, _)| state.host_is_visible(host))
    {
        let selected = state.selected_host.as_ref() == Some(host);
        let step = inspection
            .status
            .step()
            .map_or_else(|| "-".to_owned(), |step| step.to_string());
        lines.push(Line::styled(
            format!(
                "{} {host:<11} {:<11} step {step}",
                if selected { ">" } else { " " },
                format!("{:?}", inspection.status.lifecycle()).to_lowercase(),
            ),
            if selected {
                state.palette.strong()
            } else {
                state.palette.muted()
            },
        ));
    }
    render_overview_lines(frame, state, area, focused, "Negotiation", lines);
}

fn render_overview_program(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let (title, body, loaded) = overview_program_content(state);
    frame.render_widget(
        Paragraph::new(body)
            .style(if loaded {
                Style::default()
            } else {
                state.palette.muted()
            })
            .wrap(Wrap { trim: false })
            .scroll((state.program_scroll, 0))
            .block(overview_panel(title, state, area, focused)),
        area,
    );
}

pub(super) fn overview_program_content(state: &ScreenState) -> (String, String, bool) {
    let host = state
        .selected_host
        .as_ref()
        .map_or_else(|| "waiting".to_owned(), ToString::to_string);
    if let Some(snapshot) = state.active_view_snapshot() {
        let scoped_hosts = state.scoped_host_names();
        let scoped = scoped_hosts
            .iter()
            .filter_map(|host| {
                state
                    .view_history
                    .get(host)
                    .and_then(|history| {
                        history
                            .iter()
                            .find(|candidate| candidate.step == snapshot.step)
                    })
                    .map(|candidate| (host, candidate))
            })
            .collect::<Vec<_>>();
        if scoped_hosts.len() > 1 && scoped.len() != scoped_hosts.len() {
            return (
                format!(
                    "Program  step {}  INCOMPLETE {}/{}",
                    snapshot.step,
                    scoped.len(),
                    scoped_hosts.len()
                ),
                scoped_hosts
                    .iter()
                    .map(|host| {
                        let observed = scoped.iter().any(|(candidate, _)| *candidate == host);
                        format!(
                            "{host:<12} {}",
                            if observed { "observed" } else { "waiting" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                true,
            );
        }
        if scoped.len() > 1
            && scoped
                .windows(2)
                .any(|pair| pair[0].1.view != pair[1].1.view)
        {
            return (
                format!("Program  step {}  DIFFERENT", snapshot.step),
                scoped
                    .iter()
                    .map(|(host, candidate)| {
                        let summary = candidate
                            .view
                            .slots
                            .get(&Slot::Header)
                            .or_else(|| candidate.view.slots.get(&Slot::State))
                            .map_or("no program state", String::as_str)
                            .lines()
                            .next()
                            .unwrap_or("no program state");
                        format!("{host:<12} {}", sanitize::strip_ansi(summary))
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                true,
            );
        }
        let title = format!(
            "Program  {}  step {}  {}",
            if scoped_hosts.len() > 1 {
                format!("ALL {} SAME", scoped_hosts.len())
            } else {
                host
            },
            snapshot.step,
            view_history_label(state)
        );
        let body = snapshot
            .view
            .slots
            .get(&Slot::State)
            .or_else(|| snapshot.view.slots.get(&Slot::Header))
            .map_or_else(|| "No program state".to_owned(), |value| plain_slot(value));
        (title, body, true)
    } else {
        let title = format!("Program  {host}  {}", view_history_label(state));
        (title, "Waiting for the program view".to_owned(), false)
    }
}

pub(super) fn overview_program_height(state: &ScreenState, width: u16) -> u16 {
    let (_, body, _) = overview_program_content(state);
    let paragraph = Paragraph::new(body).wrap(Wrap { trim: false });
    let content_width = width.saturating_sub(2).max(1);
    u16::try_from(paragraph.line_count(content_width).saturating_add(2)).unwrap_or(u16::MAX)
}

fn render_overview_trace(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let hosts = state.scoped_host_names();
    let steps = hosts
        .iter()
        .flat_map(|host| state.host_trace(host).iter().map(|entry| entry.entry.step))
        .collect::<BTreeSet<_>>();
    let block = overview_panel(
        format!("Public trace  {} aligned steps", steps.len()),
        state,
        area,
        focused,
    );
    let available = usize::from(block.inner(area).height.saturating_sub(1));
    let rows = steps
        .iter()
        .rev()
        .take(available)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter_map(|step| {
            let observed = hosts
                .iter()
                .filter_map(|host| {
                    state
                        .host_trace(host)
                        .iter()
                        .find(|entry| entry.entry.step == step)
                        .map(|entry| (host, entry))
                })
                .collect::<Vec<_>>();
            let (_, projected) = observed.first()?;
            let entry = &projected.entry;
            let event = match &entry.event {
                PublicEvent::SessionStarted { .. } => "session started".to_owned(),
                PublicEvent::MessageReceived { from, .. } => {
                    format!("message from {}", from.fmt_short())
                }
            };
            let equal = observed.iter().skip(1).all(|(_, other)| {
                other.entry.pre_state == entry.pre_state
                    && other.entry.post_state == entry.post_state
                    && other.entry.fuel_used == entry.fuel_used
                    && other.entry.agreement.signers.count() == entry.agreement.signers.count()
            });
            let result = if observed.len() != hosts.len() {
                "INCOMPLETE"
            } else if equal {
                "SAME"
            } else {
                "DIFFERENT"
            };
            let selected = state.selected_public_position == Some(entry.step);
            Some(
                Row::new([
                    Cell::from(format!("#{:03}", entry.step)).style(state.palette.public()),
                    Cell::from(event),
                    Cell::from(format!(
                        "{} → {}",
                        entry.pre_state.fmt_short(),
                        entry.post_state.fmt_short()
                    ))
                    .style(state.palette.muted()),
                    Cell::from(format!("{}/{} {result}", observed.len(), hosts.len())).style(
                        if result == "SAME" {
                            state.palette.success()
                        } else {
                            state.palette.error()
                        },
                    ),
                ])
                .style(if selected {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default()
                }),
            )
        });
    if steps.is_empty() {
        frame.render_widget(
            Paragraph::new("No public steps yet")
                .style(state.palette.muted())
                .block(block),
            area,
        );
        return;
    }
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Fill(1),
            Constraint::Length(19),
            Constraint::Length(15),
        ],
    )
    .header(Row::new(["Step", "Event", "State edge", "Hosts"]).style(state.palette.public()))
    .column_spacing(1)
    .block(block);
    frame.render_widget(table, area);
}

fn render_overview_wasm(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let shown = state
        .inspections
        .iter()
        .filter(|(host, _)| state.host_is_visible(host))
        .map(|(_, inspection)| inspection)
        .map(|inspection| inspection.private.len())
        .sum::<usize>();
    let total = state
        .inspections
        .iter()
        .filter(|(host, _)| state.host_is_visible(host))
        .map(|(_, inspection)| inspection)
        .map(|inspection| inspection.private_total)
        .sum::<u64>();
    let hosts = state.scoped_host_names();
    let step = state.selected_public_position.or_else(|| {
        hosts
            .iter()
            .filter_map(|host| state.host_trace(host).last().map(|entry| entry.entry.step))
            .max()
    });
    let mut lines = Vec::new();
    if let Some(step) = step {
        let event = hosts
            .iter()
            .flat_map(|host| state.host_trace(host))
            .find(|entry| entry.entry.step == step)
            .map_or_else(
                || "handler observation unavailable".to_owned(),
                |entry| match &entry.entry.event {
                    PublicEvent::SessionStarted { .. } => "session started".to_owned(),
                    PublicEvent::MessageReceived { from, .. } => {
                        format!("message from {}", from.fmt_short())
                    }
                },
            );
        lines.push(Line::from(vec![
            Span::styled(format!("(pub) #{step} "), state.palette.public()),
            Span::raw(event),
        ]));
        for (index, host) in hosts.iter().enumerate() {
            let branch = if index + 1 == hosts.len() {
                "└─"
            } else {
                "├─"
            };
            let record = state.inspections.get(host).and_then(|inspection| {
                inspection
                    .private
                    .iter()
                    .rev()
                    .find(|record| record.public_position == step.saturating_add(1))
            });
            lines.push(match record {
                Some(record) => Line::from(vec![
                    Span::styled(format!(" {branch} (priv) "), state.palette.emphasis()),
                    Span::raw(format!(
                        "{host} #{} {}",
                        record.sequence,
                        wasm::private_event_name(record.event)
                    )),
                ]),
                None => Line::from(vec![
                    Span::styled(format!(" {branch} "), state.palette.muted()),
                    Span::raw(format!("{host}  no private handler observed")),
                ]),
            });
        }
    } else {
        lines.push(Line::styled(
            "Waiting for the first public handler",
            state.palette.muted(),
        ));
    }
    render_overview_lines(
        frame,
        state,
        area,
        focused,
        format!("Wasm  private visible {shown} of {total}"),
        lines,
    );
}

fn render_overview_events(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let scoped = state
        .system_events
        .iter()
        .filter(|event| {
            event
                .host
                .id
                .parse::<HostName>()
                .is_ok_and(|host| state.host_is_visible(&host))
        })
        .collect::<Vec<_>>();
    let block = overview_panel(
        format!("System events  {} Host-local observations", scoped.len()),
        state,
        area,
        focused,
    );
    let visible = usize::from(block.inner(area).height);
    let lines = scoped
        .iter()
        .rev()
        .take(visible)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|event| system_event_line(event, state))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
