//! Public and Host-private guest handler execution tree.

use super::*;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

#[derive(Debug, Clone, PartialEq, Eq)]
enum CrossingRow {
    Public {
        step: u64,
    },
    Boundary {
        after_position: u64,
    },
    Private {
        host: HostName,
        sequence: u64,
        last: bool,
    },
}

impl CrossingRow {
    fn key(&self) -> CrossingKey {
        match self {
            Self::Public { step } => CrossingKey::Public { step: *step },
            Self::Boundary { after_position } => CrossingKey::Boundary {
                after_position: *after_position,
            },
            Self::Private { host, sequence, .. } => CrossingKey::Private {
                host: host.clone(),
                sequence: *sequence,
            },
        }
    }
}

/// Return the public step preceding a private handler's public cursor.
/// Cursor zero means the handler ran before the first public step.
pub(super) const fn private_parent_step(public_position: u64) -> Option<u64> {
    public_position.checked_sub(1)
}

pub(super) fn private_cursor_label(public_position: u64) -> String {
    private_parent_step(public_position).map_or_else(
        || "before the first public step".to_owned(),
        |step| format!("after public step #{step}"),
    )
}

/// Return stable semantic selection keys in tree order.
pub(super) fn keys(state: &ScreenState) -> Vec<CrossingKey> {
    tree_rows(state).iter().map(CrossingRow::key).collect()
}

fn tree_rows(state: &ScreenState) -> Vec<CrossingRow> {
    let mut private_by_cursor = BTreeMap::<u64, Vec<(HostName, u64)>>::new();
    for host in state.scoped_host_names() {
        let Some(inspection) = state.inspections.get(&host) else {
            continue;
        };
        for entry in &inspection.private {
            private_by_cursor
                .entry(entry.public_position)
                .or_default()
                .push((host.clone(), entry.sequence));
        }
    }

    let mut rows = Vec::new();
    if let Some(children) = private_by_cursor.remove(&0) {
        rows.push(CrossingRow::Boundary { after_position: 0 });
        push_children(&mut rows, children);
    }
    for step in state.scoped_trace_steps() {
        rows.push(CrossingRow::Public { step });
        if let Some(public_position) = step.checked_add(1)
            && let Some(children) = private_by_cursor.remove(&public_position)
        {
            push_children(&mut rows, children);
        }
    }
    for (after_position, children) in private_by_cursor {
        rows.push(CrossingRow::Boundary { after_position });
        push_children(&mut rows, children);
    }
    rows
}

fn push_children(rows: &mut Vec<CrossingRow>, mut children: Vec<(HostName, u64)>) {
    children.sort();
    let last = children.len().saturating_sub(1);
    rows.extend(
        children
            .into_iter()
            .enumerate()
            .map(|(index, (host, sequence))| CrossingRow::Private {
                host,
                sequence,
                last: index == last,
            }),
    );
}

pub(super) fn row_count(state: &ScreenState) -> usize {
    keys(state).len()
}

/// Render public handlers as roots and Host-private handlers as children.
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
    let tree = tree_rows(state);
    let keys = tree.iter().map(CrossingRow::key).collect::<Vec<_>>();
    let scoped_hosts = state.scoped_host_names();
    let visible_private = scoped_hosts
        .iter()
        .filter_map(|host| state.inspections.get(host))
        .map(|inspection| inspection.private.len())
        .sum::<usize>();
    let total_private = scoped_hosts
        .iter()
        .filter_map(|host| state.inspections.get(host))
        .map(|inspection| usize::try_from(inspection.private_total).unwrap_or(usize::MAX))
        .sum::<usize>();
    let private_label = if state.inspections.is_empty() {
        "pending".to_owned()
    } else {
        format!("visible {visible_private} of {total_private}")
    };
    let page_label = state
        .primary_host()
        .and_then(|host| {
            state.inspections.get(host).map(|inspection| {
                let range = if inspection.private.is_empty() {
                    "empty".to_owned()
                } else {
                    let last = inspection
                        .private_from
                        .saturating_add(inspection.private.len().saturating_sub(1) as u64);
                    format!("#{}–#{last}", inspection.private_from)
                };
                let navigation = if inspection.private_from > 0 || inspection.private_next.is_some()
                {
                    "  < older  > newer"
                } else {
                    ""
                };
                format!(
                    "{host} page {range}/{}{navigation}",
                    inspection.private_total
                )
            })
        })
        .unwrap_or_else(|| "page pending".to_owned());
    let title = format!(
        "Wasm handlers  {} rows  {private_label}  {page_label}",
        keys.len()
    );
    let block = panel(title, state, focused);
    let rows = tree.iter().map(|row| {
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
            CrossingRow::Boundary { after_position } => (
                if *after_position == 0 {
                    "○ BEFORE FIRST PUB".to_owned()
                } else {
                    format!("○ AFTER {after_position} PUB")
                },
                String::new(),
                "public root unavailable".to_owned(),
                state.palette.muted(),
            ),
            CrossingRow::Private {
                host,
                sequence,
                last,
            } => {
                let detail = private_detail(state, host, *sequence);
                (
                    format!("{} PRIV #{sequence}", if *last { "└─" } else { "├─" }),
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
        rows,
        [
            Constraint::Length(21),
            Constraint::Length(18),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["Execution tree", "Host", "Trigger and effects"]).style(state.palette.emphasis()),
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
                            "{host}  state {} -> {}  fuel {}  agreement {}/{}",
                            entry.entry.pre_state.fmt_short(),
                            entry.entry.post_state.fmt_short(),
                            entry.entry.fuel_used,
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
        CrossingKey::Boundary { after_position } => vec![
            Line::styled(
                private_cursor_label(*after_position),
                state.palette.strong(),
            ),
            Line::styled(
                format!("{after_position} public entries had been applied"),
                state.palette.emphasis(),
            ),
            Line::styled(
                "The matching public root is not present in this projection.",
                state.palette.muted(),
            ),
        ],
        CrossingKey::Private { host, sequence } => {
            if let Some(summary) = state.inspections.get(host).and_then(|inspection| {
                inspection
                    .private
                    .iter()
                    .find(|entry| entry.sequence == *sequence)
            }) {
                let effects = if summary.effects.is_empty() {
                    "none".to_owned()
                } else {
                    summary
                        .effects
                        .iter()
                        .map(private_effect_label)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                vec![
                    Line::styled(
                        format!("PRIVATE HANDLER #{}  Host {}", summary.sequence, host),
                        state.palette.strong(),
                    ),
                    Line::styled(
                        private_cursor_label(summary.public_position),
                        state.palette.emphasis(),
                    ),
                    Line::styled(
                        format!("trigger  {}", private_event_name(summary.event)),
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
                    Line::styled(
                        format!("fuel  {}", summary.fuel_used),
                        state.palette.muted(),
                    ),
                ]
            } else {
                vec![Line::styled(
                    "Private record is no longer visible",
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
        PublicEvent::SessionStarted { ensemble } => {
            format!(
                "session started  {} participants  observed {observed}/{}  {status}",
                ensemble.len(),
                hosts.len()
            )
        }
        PublicEvent::MessageReceived { from, msg, .. } => {
            if status == "different" {
                format!(
                    "public observations differ  observed {observed}/{}",
                    hosts.len()
                )
            } else {
                format!(
                    "message from {}  {} B  observed {observed}/{}  {status}",
                    from.fmt_short(),
                    msg.len(),
                    hosts.len()
                )
            }
        }
    }
}

fn private_detail(state: &ScreenState, host: &HostName, sequence: u64) -> String {
    state
        .inspections
        .get(host)
        .and_then(|inspection| {
            inspection
                .private
                .iter()
                .find(|entry| entry.sequence == sequence)
        })
        .map_or_else(
            || "private record unavailable".to_owned(),
            |entry| {
                let effects = if entry.effects.is_empty() {
                    "none".to_owned()
                } else {
                    entry
                        .effects
                        .iter()
                        .map(private_effect_label)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!(
                    "{}  → {}    fuel {}",
                    private_event_name(entry.event),
                    effects,
                    entry.fuel_used
                )
            },
        )
}

pub(super) fn private_event_name(kind: PrivateEventKind) -> &'static str {
    match kind {
        PrivateEventKind::InputReceived => "input received",
        PrivateEventKind::TimerFired => "timer fired",
        PrivateEventKind::TypedTimerFired => "typed timer fired",
        PrivateEventKind::Signed => "signed",
        PrivateEventKind::React => "react",
    }
}

fn private_effect_label(effect: &arena0_client::api::PrivateEffectSummary) -> String {
    let kind = match effect.kind {
        PrivateEffectKind::Broadcast => "broadcast",
        PrivateEffectKind::Callout => "callout",
        PrivateEffectKind::SetTimer => "set timer",
        PrivateEffectKind::Sign => "sign",
        PrivateEffectKind::RetryInput => "retry input",
    };
    effect
        .payload_bytes
        .map_or_else(|| kind.to_owned(), |bytes| format!("{kind} {bytes} B"))
}
