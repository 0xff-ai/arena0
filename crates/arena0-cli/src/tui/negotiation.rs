use super::*;

use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table, TableState};

/// Render the negotiation detail view.
///
/// The table is keyed by the daemon's `HostName`, while the inspector keeps
/// observed event facts separate from the durable activation projection.
pub(super) fn render(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    if matches!(
        state.page.inspector().as_ref(),
        Some(OverviewPane::Negotiation)
    ) {
        render_inspector(frame, state, area);
        return;
    }

    let [summary, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(1)])
        .spacing(Spacing::Overlap(1))
        .areas(area);
    render_summary(frame, state, summary);

    let hosts = state.scoped_host_names();
    if body.width >= 108 {
        let [table_area, inspector_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
            .spacing(Spacing::Overlap(1))
            .areas(body);
        render_hosts(frame, state, table_area, &hosts, focused);
        render_host_inspector(
            frame,
            state,
            inspector_area,
            hosts.get(selected_index(state, &hosts)),
        );
    } else {
        let [table_area, inspector_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(46), Constraint::Min(1)])
            .spacing(Spacing::Overlap(1))
            .areas(body);
        render_hosts(frame, state, table_area, &hosts, focused);
        render_host_inspector(
            frame,
            state,
            inspector_area,
            hosts.get(selected_index(state, &hosts)),
        );
    }
}

fn selected_index(state: &ScreenState, hosts: &[HostName]) -> usize {
    state
        .primary_host()
        .and_then(|selected| hosts.iter().position(|host| host == selected))
        .unwrap_or(0)
}

fn render_summary(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let lifecycle = state.scoped_status_label();
    let status = state.status();
    let negotiation = status
        .and_then(|status| status.negotiation_id)
        .map_or_else(|| "pending".to_owned(), |id| id.fmt_short().to_string());
    let queue = status
        .and_then(ExecStatus::queue_position)
        .map_or_else(|| "none".to_owned(), |position| position.to_string());
    let (accepted, target) = latest_ticket_progress(state).unwrap_or((0, 0));
    let progress = if target == 0 {
        "pending".to_owned()
    } else {
        format!("{accepted}/{target}")
    };
    let selected_host = state
        .primary_host()
        .map_or_else(|| "pending".to_owned(), ToString::to_string);
    let lines = vec![
        Line::from(vec![
            Span::styled("Host ", state.palette.muted()),
            Span::styled(selected_host, state.palette.strong()),
            Span::styled("    ", state.palette.muted()),
            Span::styled("execution ", state.palette.muted()),
            Span::raw(status.map_or_else(
                || "pending".to_owned(),
                |status| status.exec_id.fmt_short().to_string(),
            )),
            Span::styled("    program ", state.palette.muted()),
            Span::raw(status.map_or_else(
                || "pending".to_owned(),
                |status| status.program_id.fmt_short().to_string(),
            )),
        ]),
        Line::from(vec![
            Span::styled("stage ", state.palette.muted()),
            Span::styled(lifecycle, lifecycle_style(state)),
            Span::styled("    negotiation ", state.palette.muted()),
            Span::raw(negotiation),
            Span::styled("    queue ", state.palette.muted()),
            Span::raw(queue),
        ]),
        Line::from(vec![
            Span::styled("tickets ", state.palette.muted()),
            Span::raw(progress),
            Span::styled("    Hosts ", state.palette.muted()),
            Span::raw(state.scoped_host_names().len().to_string()),
            Span::styled("    agreement ", state.palette.muted()),
            Span::raw(state.scoped_agreement_label()),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel("Negotiation stage", state, false)),
        area,
    );
}

fn render_hosts(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    area: Rect,
    hosts: &[HostName],
    focused: bool,
) {
    let selected_row = selected_index(state, hosts);
    let rows = hosts.iter().enumerate().map(|(index, host)| {
        let inspection = state.inspections.get(host);
        let session = inspection.and_then(|inspection| inspection.status.session());
        let receipt = session.map_or("-", |session| {
            if session.receipt_available {
                "ready"
            } else {
                "pending"
            }
        });
        let selected = index == selected_row;
        Row::new(vec![
            Cell::from(host.to_string()),
            Cell::from(inspection.map_or_else(
                || {
                    state.host_status(host).map_or_else(
                        || "waiting".to_owned(),
                        |status| format!("{:?}", status.lifecycle()).to_lowercase(),
                    )
                },
                |inspection| format!("{:?}", inspection.status.lifecycle()).to_lowercase(),
            )),
            Cell::from(
                inspection
                    .and_then(|inspection| inspection.status.step())
                    .map_or_else(|| "-".to_owned(), |step| step.to_string()),
            ),
            Cell::from(inspection.map_or_else(
                || "-".to_owned(),
                |inspection| format!("{}/{}", inspection.private.len(), inspection.private_total),
            )),
            Cell::from(receipt),
        ])
        .style(if selected {
            state.palette.strong()
        } else {
            state.palette.muted()
        })
    });
    let widths = [
        Constraint::Percentage(28),
        Constraint::Percentage(23),
        Constraint::Length(7),
        Constraint::Length(11),
        Constraint::Length(10),
    ];
    let mut table_state = TableState::default();
    if !hosts.is_empty() {
        table_state.select(Some(selected_index(state, hosts)));
    }
    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["Host", "lifecycle", "step", "private", "receipt"])
                .style(state.palette.emphasis()),
        )
        .row_highlight_style(state.palette.strong().add_modifier(Modifier::BOLD))
        .block(panel("Host observations", state, focused));
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn render_host_inspector(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    area: Rect,
    selected: Option<&HostName>,
) {
    let Some(host) = selected else {
        frame.render_widget(
            Paragraph::new("Waiting for Host inspections")
                .style(state.palette.muted())
                .block(panel("Host evidence", state, false)),
            area,
        );
        return;
    };
    let Some(inspection) = state.inspections.get(host) else {
        frame.render_widget(
            Paragraph::new("Waiting for Host inspection")
                .style(state.palette.muted())
                .block(panel("Host evidence", state, false)),
            area,
        );
        return;
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("Host ", state.palette.muted()),
        Span::styled(host.to_string(), state.palette.strong()),
    ])];
    observed_lines(&mut lines, state, host);
    durable_lines(&mut lines, state, inspection.activation.as_ref());
    participant_lines(&mut lines, state, host, inspection.activation.as_ref());
    receipt_lines(&mut lines, state, host);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(state.details_scroll).unwrap_or(u16::MAX), 0))
            .block(panel("Host evidence", state, false)),
        area,
    );
}

fn observed_lines(lines: &mut Vec<Line<'static>>, state: &ScreenState, host: &HostName) {
    let events = state.system_events.iter().filter(|frame| {
        frame.host == host.as_str()
            && frame.exec_id == state.host_status(host).map(|s| s.exec_id)
            && (frame.kind() == "negotiation.offer_seen"
                || frame.kind().starts_with("exec.negotiation."))
    });
    let mut count = 0usize;
    let mut latest = None;
    for event in events {
        count += 1;
        latest = Some(event.kind());
    }
    lines.push(Line::styled("OBSERVED", state.palette.emphasis()));
    lines.push(Line::styled(
        format!("  negotiation events  {count}"),
        state.palette.muted(),
    ));
    lines.push(Line::styled(
        format!("  latest               {}", latest.unwrap_or("none")),
        state.palette.muted(),
    ));
}

fn durable_lines(
    lines: &mut Vec<Line<'static>>,
    state: &ScreenState,
    activation: Option<&ActivationInspection>,
) {
    lines.push(Line::styled("DURABLE ACTIVATION", state.palette.emphasis()));
    let Some(activation) = activation else {
        lines.push(Line::styled(
            "  activation record    pending",
            state.palette.muted(),
        ));
        return;
    };
    lines.push(Line::styled(
        format!("  record               {:?}", activation.state).to_lowercase(),
        state.palette.muted(),
    ));
    lines.push(Line::styled(
        format!(
            "  negotiation          {}",
            activation.negotiation_id.fmt_short()
        ),
        state.palette.muted(),
    ));
    lines.push(Line::styled(
        format!(
            "  offer commitment     {}",
            activation.offer_hash.fmt_short()
        ),
        state.palette.muted(),
    ));
    lines.push(Line::styled(
        format!(
            "  initial state        {}",
            activation.initial_state.fmt_short()
        ),
        state.palette.muted(),
    ));
    lines.push(Line::styled(
        format!(
            "  session              {}",
            activation
                .session_id
                .map_or_else(|| "pending".to_owned(), |id| id.fmt_short().to_string())
        ),
        state.palette.muted(),
    ));
}

fn participant_lines(
    lines: &mut Vec<Line<'static>>,
    state: &ScreenState,
    host: &HostName,
    activation: Option<&ActivationInspection>,
) {
    lines.push(Line::styled(
        "PARTICIPANTS AND TICKETS",
        state.palette.emphasis(),
    ));
    if let Some(activation) = activation {
        for participant in &activation.participants {
            lines.push(Line::styled(
                format!(
                    "  {}                 ticket {}",
                    participant.peer_id.fmt_short(),
                    participant.ticket_hash.fmt_short()
                ),
                state.palette.muted(),
            ));
        }
        return;
    }
    let Some(session) = state.host_status(host).and_then(ExecStatus::session) else {
        let Some(exec_id) = state.host_status(host).map(|status| status.exec_id) else {
            lines.push(Line::styled(
                "  awaiting participant tickets",
                state.palette.muted(),
            ));
            return;
        };
        let mut peers = state
            .system_events
            .iter()
            .rev()
            .filter(|frame| frame.host == host.as_str() && frame.exec_id == Some(exec_id))
            .find_map(|frame| match &frame.data {
                EventData::NegotiationPeers { peers, .. } => Some(peers.clone()),
                _ => None,
            })
            .unwrap_or_default();
        for frame in state.system_events.iter().rev() {
            if frame.host != host.as_str() || frame.exec_id != Some(exec_id) {
                continue;
            }
            if let EventData::NegotiationTicketAccepted { participant, .. } = &frame.data
                && !peers.contains(participant)
            {
                peers.push(*participant);
            }
        }
        if peers.is_empty() {
            lines.push(Line::styled(
                "  awaiting participant tickets",
                state.palette.muted(),
            ));
        } else {
            for peer in peers {
                let ticket = state.system_events.iter().rev().find_map(|frame| {
                    if frame.host != host.as_str() || frame.exec_id != Some(exec_id) {
                        return None;
                    }
                    match &frame.data {
                        EventData::NegotiationTicketAccepted {
                            participant,
                            ticket_hash,
                            ..
                        } if *participant == peer => Some(ticket_hash.fmt_short().to_string()),
                        _ => None,
                    }
                });
                lines.push(Line::styled(
                    format!(
                        "  {}                 ticket {}",
                        peer.fmt_short(),
                        ticket.unwrap_or_else(|| "pending".to_owned())
                    ),
                    state.palette.muted(),
                ));
            }
        }
        return;
    };
    lines.push(Line::styled(
        format!(
            "  committed ensemble   {} participants",
            session.participants
        ),
        state.palette.muted(),
    ));
    for peer in &session.peers {
        lines.push(Line::styled(
            format!("  peer                  {}", peer.fmt_short()),
            state.palette.muted(),
        ));
    }
}

fn receipt_lines(lines: &mut Vec<Line<'static>>, state: &ScreenState, host: &HostName) {
    lines.push(Line::styled("RECEIPTS", state.palette.emphasis()));
    let receipts = state
        .receipts
        .iter()
        .filter(|receipt| &receipt.host == host);
    let mut any = false;
    for receipt in receipts {
        any = true;
        lines.push(Line::styled(
            format!(
                "  producer              {}  verified  {}",
                receipt.producer.fmt_short(),
                receipt.tier
            ),
            state.palette.muted(),
        ));
    }
    if !any {
        lines.push(Line::styled(
            "  no receipt replay observed",
            state.palette.muted(),
        ));
    }
}

fn render_inspector(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let modal = centered(
        area,
        area.width.saturating_sub(8).min(112),
        area.height.saturating_sub(4),
    );
    frame.render_widget(Clear, modal);
    let hosts = state.scoped_host_names();
    let selected = hosts.get(selected_index(state, &hosts));
    let mut lines = Vec::new();
    if let Some(host) = selected {
        if let Some(inspection) = state.inspections.get(host) {
            lines.push(Line::from(vec![
                Span::styled("Host ", state.palette.muted()),
                Span::styled(host.to_string(), state.palette.strong()),
            ]));
            observed_lines(&mut lines, state, host);
            durable_lines(&mut lines, state, inspection.activation.as_ref());
            participant_lines(&mut lines, state, host, inspection.activation.as_ref());
            receipt_lines(&mut lines, state, host);
        }
    } else {
        lines.push(Line::styled(
            "Waiting for Host inspections",
            state.palette.muted(),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(state.details_scroll).unwrap_or(u16::MAX), 0))
            .block(panel(" NEGOTIATION INSPECTOR  Esc close ", state, true)),
        modal,
    );
}

pub(super) fn latest_ticket_progress(state: &ScreenState) -> Option<(u16, u16)> {
    let host = state.primary_host()?;
    let exec_id = state.host_status(host)?.exec_id;
    state.system_events.iter().rev().find_map(|frame| {
        if frame.exec_id != Some(exec_id) || frame.host != host.as_str() {
            return None;
        }
        match &frame.data {
            EventData::NegotiationStarted { target_size } => Some((0, *target_size)),
            EventData::NegotiationTicketAccepted {
                ticket_count,
                target_size,
                ..
            }
            | EventData::NegotiationRetried {
                ticket_count,
                target_size,
                ..
            }
            | EventData::NegotiationTimedOut {
                ticket_count,
                target_size,
                ..
            } => Some((*ticket_count, *target_size)),
            _ => None,
        }
    })
}
