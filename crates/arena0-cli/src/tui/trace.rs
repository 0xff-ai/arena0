//! Public trace detail projection for the execution observatory.

use super::*;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alignment {
    Equal,
    Different,
    Incomplete,
}

struct Observation<'a> {
    step: u64,
    entries: Vec<(HostName, &'a TraceViewEntry)>,
}

/// Render the public trace aligned across the active Host scope.
pub(super) fn render(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let inspector = matches!(&state.page.inspector(), Some(OverviewPane::PublicTrace));
    let records_focused = focused && state.page.region() == DetailRegion::Records;
    let inspector_focused = focused && state.page.region() == DetailRegion::Inspector;
    if inspector && area.width >= 96 {
        let [records, details] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(57), Constraint::Percentage(43)])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        render_records(frame, state, records, records_focused);
        render_inspector(frame, state, details, inspector_focused);
    } else {
        render_records(frame, state, area, records_focused);
        if inspector && area.height >= 9 {
            let modal = crate::ui::centered(area, 84, 14);
            frame.render_widget(Clear, modal);
            render_inspector(frame, state, modal, inspector_focused);
        }
    }
}

fn observations<'a>(state: &'a ScreenState) -> Vec<Observation<'a>> {
    let hosts = state.scoped_host_names();
    state
        .scoped_trace_steps()
        .into_iter()
        .map(|step| Observation {
            step,
            entries: hosts
                .iter()
                .filter_map(|host| {
                    state
                        .host_trace(host)
                        .iter()
                        .find(|entry| entry.entry.step == step)
                        .map(|entry| (host.clone(), entry))
                })
                .collect(),
        })
        .collect()
}

fn alignment(observation: &Observation<'_>, expected: usize) -> Alignment {
    if observation.entries.len() < expected {
        Alignment::Incomplete
    } else if observation
        .entries
        .windows(2)
        .all(|entries| entries[0].1.entry == entries[1].1.entry)
    {
        Alignment::Equal
    } else {
        Alignment::Different
    }
}

fn alignment_label(alignment: Alignment, observed: usize, expected: usize) -> String {
    match alignment {
        Alignment::Equal => format!("observed {observed}/{expected}  equal"),
        Alignment::Different => format!("observed {observed}/{expected}  different"),
        Alignment::Incomplete => format!("observed {observed}/{expected}  incomplete"),
    }
}

fn render_records(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let hosts = state.scoped_host_names();
    let rows_data = observations(state);
    let expected = hosts.len();
    let title = format!(
        "PUBLIC TRACE  {} Hosts  {} steps  {}",
        expected,
        rows_data.len(),
        host_scope_label(state)
    );
    let block = panel(title, state, focused);
    let rows = rows_data.iter().map(|observation| {
        let status = alignment(observation, expected);
        let (event, state_edge) = if let Some((_, entry)) = observation.entries.first() {
            if status == Alignment::Different {
                (
                    "Host observations differ".to_owned(),
                    "state edges differ".to_owned(),
                )
            } else {
                (
                    trace_event_label(&entry.entry),
                    format!(
                        "{} -> {}",
                        entry.entry.pre_state.fmt_short(),
                        entry.entry.post_state.fmt_short()
                    ),
                )
            }
        } else {
            ("no observation".to_owned(), "none".to_owned())
        };
        let selected = state.selected_public_position == Some(observation.step);
        Row::new([
            Cell::from(format!(
                "{}#{:03}",
                if selected { ">" } else { " " },
                observation.step
            )),
            Cell::from(event),
            Cell::from(state_edge),
            Cell::from(alignment_label(status, observation.entries.len(), expected)),
            Cell::from(
                observation
                    .entries
                    .first()
                    .map_or_else(String::new, |(_, entry)| entry.entry.fuel_used.to_string()),
            ),
        ])
        .style(if selected {
            state.palette.strong()
        } else if status == Alignment::Different {
            state.palette.error()
        } else if status == Alignment::Incomplete {
            state.palette.muted()
        } else {
            Style::default()
        })
    });
    let mut table_state = TableState::default();
    table_state.select(state.selected_public_position.and_then(|selected| {
        rows_data
            .iter()
            .position(|observation| observation.step == selected)
    }));
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(25),
            Constraint::Length(21),
            Constraint::Length(28),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(["Step", "Event", "State edge", "Host alignment", "Fuel used"])
            .style(state.palette.public()),
    )
    .block(block)
    .column_spacing(1)
    .row_highlight_style(state.palette.strong().add_modifier(Modifier::BOLD));
    if rows_data.is_empty() {
        frame.render_widget(
            Paragraph::new("No public steps observed in this Host scope")
                .style(state.palette.muted())
                .block(panel("Public trace  waiting", state, focused)),
            area,
        );
    } else {
        frame.render_stateful_widget(table, area, &mut table_state);
    }
}

fn render_inspector(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let Some(step) = state.selected_public_position else {
        frame.render_widget(
            Paragraph::new("Select a public step to inspect")
                .style(state.palette.muted())
                .block(panel(" TRACE INSPECTOR ", state, focused)),
            area,
        );
        return;
    };
    let hosts = state.scoped_host_names();
    let Some(observation) = observations(state)
        .into_iter()
        .find(|observation| observation.step == step)
    else {
        frame.render_widget(
            Paragraph::new("Selected public step is no longer visible")
                .style(state.palette.muted())
                .block(panel(" TRACE INSPECTOR ", state, focused)),
            area,
        );
        return;
    };
    let status = alignment(&observation, hosts.len());
    let mut lines = vec![
        Line::styled(format!("PUBLIC STEP #{step}"), state.palette.strong()),
        Line::styled(
            alignment_label(status, observation.entries.len(), hosts.len()),
            state.palette.emphasis(),
        ),
    ];
    for host in hosts {
        if let Some((_, entry)) = observation
            .entries
            .iter()
            .find(|(candidate, _)| candidate == &host)
        {
            lines.push(Line::styled(
                format!(
                    "{host}  {}  state {} -> {}  fuel {}",
                    trace_event_label(&entry.entry),
                    entry.entry.pre_state.fmt_short(),
                    entry.entry.post_state.fmt_short(),
                    entry.entry.fuel_used
                ),
                state.palette.muted(),
            ));
            lines.push(Line::styled(
                format!(
                    "  agreement {}/{}  witness {}  effects {}",
                    entry.entry.agreement.signers.count(),
                    state.config.host_count(),
                    entry
                        .entry
                        .witness
                        .as_ref()
                        .map_or_else(|| "none".to_owned(), |witness| short_hex(&witness.0)),
                    trace_effect_labels(&entry.entry)
                ),
                state.palette.muted(),
            ));
            if let Some(message) = &entry.message {
                lines.push(Line::styled(
                    match message {
                        Ok(value) => format!("  decoded JSON  {}", crate::ui::compact_json(value)),
                        Err(reason) => format!("  decoded JSON  unavailable: {reason}"),
                    },
                    state.palette.muted(),
                ));
            }
        } else {
            lines.push(Line::styled(
                format!("{host}  no observation"),
                state.palette.muted(),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(state.details_scroll).unwrap_or(u16::MAX), 0))
            .block(panel(format!(" TRACE INSPECTOR  #{step} "), state, focused)),
        area,
    );
}

fn trace_event_label(entry: &TraceEntry) -> String {
    match &entry.event {
        PublicEvent::SessionStarted { ensemble } => {
            format!("session started  {} participants", ensemble.len())
        }
        PublicEvent::MessageReceived {
            message_id,
            from,
            msg,
            ..
        } => format!(
            "message {} from {}  {} B",
            message_id.fmt_short(),
            from.fmt_short(),
            msg.len()
        ),
    }
}

fn trace_effect_labels(entry: &TraceEntry) -> String {
    if entry.effects.is_empty() {
        return "none".to_owned();
    }
    entry
        .effects
        .iter()
        .map(|effect| match effect {
            PublicEffect::SessionEnd { .. } => "session end",
            PublicEffect::SessionAbort { .. } => "session abort",
            PublicEffect::Fail { .. } => "fail",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn short_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
