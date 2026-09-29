//! Public trace and Host-local guest event records.

use super::*;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

#[derive(Debug, Clone, PartialEq, Eq)]
enum CrossingRow {
    Public { step: u64 },
    Event { host: HostName, event_position: u64 },
}

impl CrossingRow {
    fn key(&self) -> CrossingKey {
        match self {
            Self::Public { step } => CrossingKey::Public { step: *step },
            Self::Event {
                host,
                event_position,
            } => CrossingKey::Event {
                host: host.clone(),
                event_position: *event_position,
            },
        }
    }
}

/// Return stable semantic selection keys in flat trace/event order.
pub(super) fn keys(state: &ScreenState) -> Vec<CrossingKey> {
    rows(state).iter().map(CrossingRow::key).collect()
}

fn rows(state: &ScreenState) -> Vec<CrossingRow> {
    let mut events = Vec::<(HostName, u64)>::new();
    for host in state.scoped_host_names() {
        let Some(inspection) = state.inspections.get(&host) else {
            continue;
        };
        for entry in &inspection.events {
            events.push((host.clone(), entry.event_position));
        }
    }
    events.sort();

    let mut rows = Vec::new();
    for step in state.scoped_trace_steps() {
        rows.push(CrossingRow::Public { step });
    }
    rows.extend(
        events
            .into_iter()
            .map(|(host, event_position)| CrossingRow::Event {
                host,
                event_position,
            }),
    );
    rows
}

pub(super) fn row_count(state: &ScreenState) -> usize {
    keys(state).len()
}

/// Render public trace entries and Host-local event records as one flat list.
pub(super) fn render(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let inspector = matches!(&state.page.inspector(), Some(OverviewPane::Wasm));
    let records_focused = focused && state.page.region() == DetailRegion::Records;
    let inspector_focused = focused && state.page.region() == DetailRegion::Inspector;
    if inspector && area.width >= 96 {
        let [records, details] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(59), Constraint::Percentage(41)])
            .spacing(Spacing::Overlap(1))
            .areas(area);
        render_records(frame, state, records, records_focused);
        render_inspector(frame, state, details, inspector_focused);
    } else {
        render_records(frame, state, area, records_focused);
        if inspector && area.height >= 9 {
            let modal = crate::ui::centered(area, 84, 15);
            frame.render_widget(Clear, modal);
            render_inspector(frame, state, modal, inspector_focused);
        }
    }
}

fn render_records(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let records = rows(state);
    let keys = records.iter().map(CrossingRow::key).collect::<Vec<_>>();
    let scoped_hosts = state.scoped_host_names();
    let visible_events = scoped_hosts
        .iter()
        .filter_map(|host| state.inspections.get(host))
        .map(|inspection| inspection.events.len())
        .sum::<usize>();
    let total_events = scoped_hosts
        .iter()
        .filter_map(|host| state.inspections.get(host))
        .map(|inspection| usize::try_from(inspection.events_total).unwrap_or(usize::MAX))
        .sum::<usize>();
    let events_label = if state.inspections.is_empty() {
        "pending".to_owned()
    } else {
        format!("visible {visible_events} of {total_events}")
    };
    let page_label = state
        .primary_host()
        .and_then(|host| {
            state.inspections.get(host).map(|inspection| {
                let range = if inspection.events.is_empty() {
                    "empty".to_owned()
                } else {
                    let last = inspection
                        .events_from
                        .saturating_add(inspection.events.len().saturating_sub(1) as u64);
                    format!("#{}–#{last}", inspection.events_from)
                };
                let navigation = if inspection.events_from > 0 || inspection.events_next.is_some() {
                    "  < older  > newer"
                } else {
                    ""
                };
                format!(
                    "{host} page {range}/{}{navigation}",
                    inspection.events_total
                )
            })
        })
        .unwrap_or_else(|| "page pending".to_owned());
    let title = format!(
        "Wasm handlers  {} rows  {events_label}  {page_label}",
        records.len()
    );
    let block = panel(title, state, focused);
    let table_rows = records.iter().map(|row| {
        let key = row.key();
        let (label, host, detail, style) = match row {
            CrossingRow::Public { step } => {
                let scoped_hosts = state.scoped_host_names();
                let observed = scoped_hosts
                    .iter()
                    .filter(|host| {
                        state
                            .host_trace(host)
                            .iter()
                            .any(|entry| entry.entry.step == *step)
                    })
                    .count();
                let detail = state
                    .scoped_host_names()
                    .iter()
                    .flat_map(|host| state.host_trace(host).iter())
                    .find(|entry| entry.entry.step == *step)
                    .map_or_else(
                        || "public entry unavailable".to_owned(),
                        |entry| public_detail(state, *step, &entry.entry),
                    );
                (
                    format!("● PUB #{step}"),
                    format!("public {observed}/{}", scoped_hosts.len()),
                    detail,
                    state.palette.public(),
                )
            }
            CrossingRow::Event {
                host,
                event_position,
            } => {
                let detail = event_detail(state, host, *event_position);
                (
                    format!("EVENT #{event_position}"),
                    host.to_string(),
                    detail,
                    state.palette.emphasis(),
                )
            }
        };
        let selected = state.selected_crossing.as_ref() == Some(&key);
        Row::new([Cell::from(label), Cell::from(host), Cell::from(detail)]).style(if selected {
            state.palette.strong()
        } else {
            style
        })
    });
    let mut table_state = TableState::default();
    table_state.select(
        state
            .selected_crossing
            .as_ref()
            .and_then(|selected| keys.iter().position(|key| key == selected)),
    );
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(21),
            Constraint::Length(18),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["Trace / event", "Host", "Trigger and effects"]).style(state.palette.emphasis()),
    )
    .block(block)
    .column_spacing(1)
    .row_highlight_style(state.palette.strong().add_modifier(Modifier::BOLD));
    if keys.is_empty() {
        frame.render_widget(
            Paragraph::new("No guest handler calls yet")
                .style(state.palette.muted())
                .block(panel("Wasm handlers  waiting", state, focused)),
            area,
        );
    } else {
        frame.render_stateful_widget(table, area, &mut table_state);
    }
}

fn render_inspector(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let Some(selected) = state.selected_crossing.as_ref() else {
        frame.render_widget(
            Paragraph::new("Select a handler to inspect")
                .style(state.palette.muted())
                .block(panel(" HANDLER INSPECTOR ", state, focused)),
            area,
        );
        return;
    };
    let lines = match selected {
        CrossingKey::Public { step } => {
            let hosts = state.scoped_host_names();
            let entries = hosts
                .iter()
                .filter_map(|host| {
                    state
                        .host_trace(host)
                        .iter()
                        .find(|entry| entry.entry.step == *step)
                        .map(|entry| (host, entry))
                })
                .collect::<Vec<_>>();
            if let Some((_, representative)) = entries.first() {
                let mut lines = vec![
                    Line::styled(format!("PUBLIC STEP #{step}"), state.palette.strong()),
                    Line::styled(
                        public_detail(state, *step, &representative.entry),
                        state.palette.emphasis(),
                    ),
                ];
                for (host, entry) in entries {
                    lines.push(Line::styled(
                        format!(
                            "{host}  state {} -> {}  agreement {}/{}",
                            entry.entry.pre_state.fmt_short(),
                            entry.entry.post_state.fmt_short(),
                            entry.entry.agreement.signers.count(),
                            state.config.host_count()
                        ),
                        state.palette.muted(),
                    ));
                }
                lines
            } else {
                vec![
                    Line::styled(format!("PUBLIC #{step}"), state.palette.strong()),
                    Line::styled("Public entry is no longer visible", state.palette.muted()),
                ]
            }
        }
        CrossingKey::Event {
            host,
            event_position,
        } => {
            if let Some(summary) = state.inspections.get(host).and_then(|inspection| {
                inspection
                    .events
                    .iter()
                    .find(|entry| entry.event_position == *event_position)
            }) {
                let effects = if summary.effects.is_empty() {
                    "none".to_owned()
                } else {
                    summary
                        .effects
                        .iter()
                        .map(effect_label)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                vec![
                    Line::styled(
                        format!("EVENT #{}  Host {}", summary.event_position, host),
                        state.palette.strong(),
                    ),
                    Line::styled(
                        format!(
                            "agreed steps  {}",
                            if summary.agreed_steps.is_empty() {
                                "none".to_owned()
                            } else {
                                summary
                                    .agreed_steps
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        ),
                        state.palette.emphasis(),
                    ),
                    Line::styled(
                        format!("event  {}", event_name(summary.event)),
                        state.palette.muted(),
                    ),
                    Line::styled(
                        format!(
                            "input size  {}",
                            summary
                                .input_payload_bytes
                                .map_or_else(|| "none".to_owned(), |bytes| format!("{bytes} B"))
                        ),
                        state.palette.muted(),
                    ),
                    Line::styled(format!("effects  {effects}"), state.palette.muted()),
                ]
            } else {
                vec![Line::styled(
                    "Event record is no longer visible",
                    state.palette.muted(),
                )]
            }
        }
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(state.details_scroll).unwrap_or(u16::MAX), 0))
            .block(panel(" HANDLER INSPECTOR ", state, focused)),
        area,
    );
}

fn public_detail(state: &ScreenState, step: u64, representative: &TraceEntry) -> String {
    let hosts = state.scoped_host_names();
    let entries = hosts
        .iter()
        .filter_map(|host| {
            state
                .host_trace(host)
                .iter()
                .find(|entry| entry.entry.step == step)
        })
        .collect::<Vec<_>>();
    let observed = entries.len();
    let status = if observed < hosts.len() {
        "incomplete"
    } else if entries
        .windows(2)
        .all(|entries| entries[0].entry == entries[1].entry)
    {
        "equal"
    } else {
        "different"
    };
    match &representative.event {
        TraceEvent::SessionStarted { ensemble } => {
            format!(
                "session started  {} participants  observed {observed}/{}  {status}",
                ensemble.len(),
                hosts.len()
            )
        }
        TraceEvent::Message { from, data } => {
            if status == "different" {
                format!(
                    "public observations differ  observed {observed}/{}",
                    hosts.len()
                )
            } else {
                format!(
                    "message from {}  {} B  observed {observed}/{}  {status}",
                    from.fmt_short(),
                    data.len(),
                    hosts.len()
                )
            }
        }
    }
}

fn event_detail(state: &ScreenState, host: &HostName, event_position: u64) -> String {
    state
        .inspections
        .get(host)
        .and_then(|inspection| {
            inspection
                .events
                .iter()
                .find(|entry| entry.event_position == event_position)
        })
        .map_or_else(
            || "event record unavailable".to_owned(),
            |entry| {
                let effects = if entry.effects.is_empty() {
                    "none".to_owned()
                } else {
                    entry
                        .effects
                        .iter()
                        .map(effect_label)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!("{}  → {}", event_name(entry.event), effects,)
            },
        )
}

pub(super) fn event_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::SessionStarted => "session started",
        EventKind::MessageReceived => "message received",
        EventKind::InputReceived => "input received",
        EventKind::TimerFired => "timer fired",
    }
}

fn effect_label(effect: &arena0_client::api::EffectSummary) -> String {
    let kind = match effect.kind {
        EffectKind::SessionEnd => "session end",
        EffectKind::SessionAbort => "session abort",
        EffectKind::Broadcast => "broadcast",
        EffectKind::SetTimer => "set timer",
        EffectKind::Fail => "fail",
    };
    effect
        .payload_bytes
        .map_or_else(|| kind.to_owned(), |bytes| format!("{kind} {bytes} B"))
}
