//! Host-local semantic event summaries.

use super::*;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

/// Stable semantic keys for the bounded event projection.
pub(super) fn keys(state: &ScreenState) -> Vec<EventKey> {
    state
        .system_events
        .iter()
        .filter(|event| visible_event(state, event))
        .map(|event| EventKey {
            host: event.host.id.clone(),
            boot_id: event.boot_id.clone(),
            seq: event.seq,
        })
        .collect()
}

pub(super) fn row_count(state: &ScreenState) -> usize {
    keys(state).len()
}

fn visible_event(state: &ScreenState, event: &EventFrame) -> bool {
    state
        .scoped_host_names()
        .iter()
        .any(|host| host.as_str() == event.host.id)
}

pub(super) fn host_label(host: &arena0_client::api::HostInfo) -> String {
    match host.user_agent.as_deref() {
        Some(user_agent) if !user_agent.is_empty() => format!("{} ({user_agent})", host.id),
        _ => host.id.clone(),
    }
}

fn scoped_events(state: &ScreenState) -> Vec<&EventFrame> {
    state
        .system_events
        .iter()
        .filter(|event| visible_event(state, event))
        .collect()
}

/// Render bounded Host-local events. The stream intentionally makes no claim
/// about global order or durable audit-log semantics.
pub(super) fn render(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let inspector = matches!(&state.page.inspector(), Some(OverviewPane::SystemEvents));
    let records_focused = focused && state.page.region() == DetailRegion::Records;
    let inspector_focused = focused && state.page.region() == DetailRegion::Inspector;
    if matches!(&state.host_scope, HostScope::Compare { .. }) {
        render_compare(frame, state, area, records_focused);
        if inspector && area.height >= 9 {
            let modal = centered(area, 84, 15);
            frame.render_widget(Clear, modal);
            render_inspector(frame, state, modal, inspector_focused);
        }
        return;
    }
    if inspector && area.width >= 96 {
        let [records, details] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        render_records(frame, state, records, records_focused);
        render_inspector(frame, state, details, inspector_focused);
    } else {
        render_records(frame, state, area, records_focused);
        if inspector && area.height >= 9 {
            let modal = centered(area, 84, 15);
            frame.render_widget(Clear, modal);
            render_inspector(frame, state, modal, inspector_focused);
        }
    }
}

fn render_compare(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let hosts = state.scoped_host_names();
    if hosts.is_empty() {
        frame.render_widget(
            Paragraph::new("Waiting for the two Host streams")
                .style(state.palette.muted())
                .block(panel("System events  compare", state, focused)),
            area,
        );
        return;
    }
    let direction = if area.width >= 96 {
        Direction::Horizontal
    } else {
        Direction::Vertical
    };
    let share = 100 / u16::try_from(hosts.len()).unwrap_or(1);
    let columns = Layout::default()
        .direction(direction)
        .constraints(vec![Constraint::Percentage(share); hosts.len()])
        .spacing(Spacing::Overlap(1))
        .split(area);
    for (host, column) in hosts.iter().zip(columns.iter().copied()) {
        render_host_events(frame, state, column, focused, host);
    }
}

fn render_host_events(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    area: Rect,
    focused: bool,
    host: &HostName,
) {
    let events = state
        .system_events
        .iter()
        .filter(|event| event.host.id == host.as_str())
        .collect::<Vec<_>>();
    let keys = events
        .iter()
        .map(|event| EventKey {
            host: event.host.id.clone(),
            boot_id: event.boot_id.clone(),
            seq: event.seq,
        })
        .collect::<Vec<_>>();
    let rows = events.iter().map(|event| {
        let key = EventKey {
            host: event.host.id.clone(),
            boot_id: event.boot_id.clone(),
            seq: event.seq,
        };
        let selected = state.selected_event.as_ref() == Some(&key);
        Row::new([
            Cell::from(event_time(event.ts)),
            Cell::from(event.seq.to_string()),
            Cell::from(event.kind()),
            Cell::from(event_summary(event)),
        ])
        .style(if selected {
            state.palette.strong()
        } else {
            Style::default()
        })
    });
    let mut table_state = TableState::default();
    table_state.select(
        state
            .selected_event
            .as_ref()
            .and_then(|selected| keys.iter().position(|key| key == selected)),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(26),
            Constraint::Min(18),
        ],
    )
    .header(Row::new(["Local time", "Seq", "Kind", "Details"]).style(state.palette.emphasis()))
    .block(panel(
        format!("EVENTS  {host}  local sequence only"),
        state,
        focused,
    ))
    .column_spacing(1)
    .row_highlight_style(state.palette.strong().add_modifier(Modifier::BOLD));
    if events.is_empty() {
        frame.render_widget(
            Paragraph::new("No events observed for this Host")
                .style(state.palette.muted())
                .block(panel(format!("Events  {host}"), state, focused)),
            area,
        );
    } else {
        frame.render_stateful_widget(table, area, &mut table_state);
    }
}

fn render_records(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let events = scoped_events(state);
    let ordering = match &state.host_scope {
        HostScope::All => "merged observed stream; not causal order",
        HostScope::One(host) => return render_host_events(frame, state, area, focused, host),
        HostScope::Compare { .. } => "Host-local columns",
    };
    let title = if events.len() == MAX_SYSTEM_EVENTS {
        format!("SYSTEM EVENTS  bounded {}  {ordering}", MAX_SYSTEM_EVENTS,)
    } else {
        format!("SYSTEM EVENTS  {} observed  {ordering}", events.len(),)
    };
    let block = panel(title, state, focused);
    let keys = keys(state);
    let rows = events.iter().enumerate().map(|(index, event)| {
        let key = &keys[index];
        let selected = state.selected_event.as_ref() == Some(key);
        Row::new([
            Cell::from(event_time(event.ts)),
            Cell::from(host_label(&event.host)),
            Cell::from(format!("{}", event.seq)),
            Cell::from(event.kind()),
            Cell::from(event_summary(event)),
        ])
        .style(if selected {
            state.palette.strong()
        } else {
            Style::default()
        })
    });
    let mut table_state = TableState::default();
    table_state.select(
        state
            .selected_event
            .as_ref()
            .and_then(|selected| keys.iter().position(|key| key == selected)),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(14),
            Constraint::Length(7),
            Constraint::Length(26),
            Constraint::Min(18),
        ],
    )
    .header(
        Row::new(["Local time", "Host", "Seq", "Kind", "Event summary"])
            .style(state.palette.emphasis()),
    )
    .block(block)
    .column_spacing(1)
    .row_highlight_style(state.palette.strong().add_modifier(Modifier::BOLD));
    if events.is_empty() {
        frame.render_widget(
            Paragraph::new("Waiting for Host events")
                .style(state.palette.muted())
                .block(panel("System events  waiting", state, focused)),
            area,
        );
    } else {
        frame.render_stateful_widget(table, area, &mut table_state);
    }
}

fn render_inspector(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let Some(key) = state.selected_event.as_ref() else {
        frame.render_widget(
            Paragraph::new("Select an event to inspect")
                .style(state.palette.muted())
                .block(panel(" EVENT INSPECTOR ", state, focused)),
            area,
        );
        return;
    };
    let Some(event) = state.system_events.iter().find(|event| {
        visible_event(state, event)
            && event.host.id == key.host
            && event.boot_id == key.boot_id
            && event.seq == key.seq
    }) else {
        frame.render_widget(
            Paragraph::new("Event is no longer visible")
                .style(state.palette.muted())
                .block(panel(" EVENT INSPECTOR ", state, focused)),
            area,
        );
        return;
    };
    let mut lines = vec![
        Line::styled(
            format!("{}  {}", event.kind(), host_label(&event.host)),
            state.palette.strong(),
        ),
        Line::styled(
            format!("boot {}  local sequence {}", event.boot_id, event.seq),
            state.palette.muted(),
        ),
        Line::styled(
            format!("observed at {}", event_time(event.ts)),
            state.palette.muted(),
        ),
    ];
    if event.exec_id.is_some() || event.session_id.is_some() {
        lines.push(Line::from(vec![
            Span::styled("execution  ", state.palette.muted()),
            Span::raw(
                event
                    .exec_id
                    .map_or_else(|| "none".to_owned(), |id| id.fmt_short().to_string()),
            ),
            Span::styled("    session  ", state.palette.muted()),
            Span::raw(
                event
                    .session_id
                    .map_or_else(|| "none".to_owned(), |id| id.fmt_short().to_string()),
            ),
        ]));
    }
    lines.push(Line::styled(
        format!("summary  {}", event_summary(event)),
        state.palette.emphasis(),
    ));
    if let Some(fields) = omitted_fields(event) {
        lines.push(Line::styled(
            format!("not shown  {fields}"),
            state.palette.muted(),
        ));
    }
    lines.push(Line::styled(
        "This is a bounded Host-local stream, not a global order or durable audit log.",
        state.palette.muted(),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(state.details_scroll).unwrap_or(u16::MAX), 0))
            .block(panel(" EVENT INSPECTOR ", state, focused)),
        area,
    );
}

pub(super) fn event_summary(event: &EventFrame) -> String {
    match &event.data {
        EventData::HostStarted { abi_version, .. } => format!(
            "peer {}  ua {}  ABI {}",
            event.host.peer_id.fmt_short(),
            event.host.user_agent.as_deref().unwrap_or("(none)"),
            abi_version
        ),
        EventData::HostStopped {
            reason,
            uptime_secs,
        } => format!(
            "uptime {}s  {}",
            uptime_secs,
            reason.as_deref().unwrap_or("stopped")
        ),
        EventData::OfferSeen {
            creator, offer_seq, ..
        } => format!("creator {}  offer {}", creator.fmt_short(), offer_seq),
        EventData::Created {
            origin,
            queue_position,
            ..
        } => format!("origin {:?}  queue {:?}", origin, queue_position),
        EventData::Terminated {
            reason,
            failed_class,
        } => format!("{}  class {:?}", reason, failed_class),
        EventData::NegotiationStarted { target_size } => format!("target {}", target_size),
        EventData::NegotiationOfferAccepted { creator, offer_seq } => {
            format!("creator {}  offer {}", creator.fmt_short(), offer_seq)
        }
        EventData::NegotiationTicketAccepted {
            ticket_count,
            target_size,
            participant,
            ..
        } => format!(
            "participant {}  tickets {}/{}",
            participant.fmt_short(),
            ticket_count,
            target_size
        ),
        EventData::NegotiationPeers { lifecycle, peers } => {
            format!("{} peers  {:?}", peers.len(), lifecycle)
        }
        EventData::NegotiationPrepared { participants }
        | EventData::NegotiationResumed { participants }
        | EventData::NegotiationCommitted { participants } => {
            format!("{} participants", participants)
        }
        EventData::NegotiationRetried {
            attempt,
            ticket_count,
            sig_count,
            target_size,
            ..
        } => format!(
            "attempt {}  tickets {}/{}  agreements {}/{}",
            attempt, ticket_count, target_size, sig_count, target_size
        ),
        EventData::NegotiationRejoined {} => "rejoined".to_owned(),
        EventData::NegotiationTimedOut {
            ticket_count,
            sig_count,
            target_size,
            ..
        } => format!(
            "tickets {}/{}  agreements {}/{}",
            ticket_count, target_size, sig_count, target_size
        ),
        EventData::SessionStarted { ensemble } => format!("{} participants", ensemble.len()),
        EventData::SessionCallout { name, .. } => format!("callout {}", name),
        EventData::SessionCalloutAnswered { pending_id } => {
            format!("pending {} answered", pending_id)
        }
        EventData::SessionStep {
            step,
            fuel_used,
            signers,
            participants,
            ..
        } => format!(
            "step {}  fuel {}  agreement {}/{}",
            step, fuel_used, signers, participants
        ),
        EventData::SessionEnded { terminal } => match terminal {
            SessionTerminal::Completed { .. } => "completed".to_owned(),
            SessionTerminal::Aborted { step, reason } => format!("aborted at {}  {}", step, reason),
        },
        EventData::Lagged { skipped } => format!("{} events dropped", skipped),
    }
}

fn omitted_fields(event: &EventFrame) -> Option<&'static str> {
    match &event.data {
        EventData::HostStarted { .. } => Some("version, transport key, ABI"),
        EventData::OfferSeen { .. } => Some("program ID, negotiation ID"),
        EventData::Created { .. } => Some("program ID, negotiation ID"),
        EventData::NegotiationTicketAccepted { .. } => Some("ticket hash"),
        EventData::NegotiationPeers { .. } => Some("peer identities"),
        EventData::NegotiationRetried { .. } | EventData::NegotiationTimedOut { .. } => {
            Some("negotiation stage")
        }
        EventData::SessionStarted { .. } => Some("participant identities"),
        EventData::SessionCallout { .. } => {
            Some("pending ID, callout index, prompt, schema, context")
        }
        EventData::SessionStep { .. } => Some("pre-state hash, post-state hash"),
        EventData::SessionEnded {
            terminal: SessionTerminal::Completed { .. },
        } => Some("program outcome"),
        EventData::HostStopped { .. }
        | EventData::Terminated { .. }
        | EventData::NegotiationStarted { .. }
        | EventData::NegotiationOfferAccepted { .. }
        | EventData::NegotiationPrepared { .. }
        | EventData::NegotiationResumed { .. }
        | EventData::NegotiationCommitted { .. }
        | EventData::NegotiationRejoined {}
        | EventData::SessionCalloutAnswered { .. }
        | EventData::SessionEnded {
            terminal: SessionTerminal::Aborted { .. },
        }
        | EventData::Lagged { .. } => None,
    }
}

pub(super) fn event_time(timestamp_ms: u64) -> String {
    let seconds = (timestamp_ms / 1_000) % (24 * 60 * 60);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        (seconds / 60) % 60,
        seconds % 60
    )
}
