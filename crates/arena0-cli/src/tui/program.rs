use super::*;

use ratatui::widgets::{Clear, Paragraph};

/// Render the complete guest-owned program view for the selected Host and step.
pub(super) fn render(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    if matches!(state.page.inspector().as_ref(), Some(OverviewPane::Program)) {
        render_inspector(frame, state, area);
        return;
    }

    let host_names = state.scoped_host_names();
    let selected_host = selected_host(state, &host_names);
    if matches!(&state.host_scope, HostScope::Compare { .. }) {
        render_compare(frame, state, area, focused);
        return;
    }
    render_guest_view(frame, state, area, selected_host, focused);
}

fn selected_host<'a>(state: &ScreenState, hosts: &'a [HostName]) -> Option<&'a HostName> {
    state
        .primary_host()
        .and_then(|selected| hosts.iter().find(|host| *host == selected))
        .or_else(|| hosts.first())
}

fn render_compare(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, focused: bool) {
    let block = panel("Program comparison", state, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let scoped = match &state.host_scope {
        HostScope::Compare { left, right } => vec![left, right],
        HostScope::All | HostScope::One(_) => Vec::new(),
    };
    if scoped.len() < 2 {
        frame.render_widget(
            Paragraph::new("Compare needs two visible Hosts")
                .style(state.palette.muted())
                .block(panel("Program projections", state, false)),
            inner,
        );
        return;
    }
    let [left, right] = Layout::default()
        .direction(if inner.width >= 96 {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .spacing(Spacing::Overlap(1))
        .areas(inner);
    render_guest_view(frame, state, left, Some(scoped[0]), false);
    render_guest_view(frame, state, right, Some(scoped[1]), false);
}

fn selected_snapshot<'a>(
    state: &'a ScreenState,
    host: Option<&HostName>,
) -> Option<&'a ViewSnapshot> {
    let host = host?;
    let step = state.active_view_snapshot()?.step;
    state
        .view_history
        .get(host)
        .and_then(|history| history.iter().find(|snapshot| snapshot.step == step))
}

fn render_guest_view(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    area: Rect,
    host: Option<&HostName>,
    focused: bool,
) {
    let Some(snapshot) = selected_snapshot(state, host) else {
        frame.render_widget(
            Paragraph::new("Waiting for the program view")
                .style(state.palette.muted())
                .block(panel("Program view", state, focused)),
            area,
        );
        return;
    };
    let view = &snapshot.view;
    let step_title = format!(
        "Program view  guest output  Host {}  exact step {}",
        snapshot.host, snapshot.step
    );
    let block = panel(step_title, state, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 6 {
        let [header, agents, state_area, status] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(inner);
        render_inline_slot(
            frame,
            state,
            header,
            view,
            Slot::Header,
            "Header",
            state.palette.emphasis(),
        );
        render_inline_slot(
            frame,
            state,
            agents,
            view,
            Slot::Agents,
            "Agents",
            state.palette.strong(),
        );
        render_inline_slot(
            frame,
            state,
            state_area,
            view,
            Slot::State,
            "State",
            Style::default(),
        );
        render_inline_slot(
            frame,
            state,
            status,
            view,
            Slot::StatusBar,
            "StatusBar",
            state.palette.muted(),
        );
        return;
    }

    let [header, agents, state_label, state_area, status] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
    render_inline_slot(
        frame,
        state,
        header,
        view,
        Slot::Header,
        "Header",
        state.palette.emphasis(),
    );
    render_inline_slot(
        frame,
        state,
        agents,
        view,
        Slot::Agents,
        "Agents",
        state.palette.strong(),
    );
    frame.render_widget(
        Paragraph::new("State").style(state.palette.emphasis()),
        state_label,
    );
    render_state(frame, state, state_area, view);
    render_inline_slot(
        frame,
        state,
        status,
        view,
        Slot::StatusBar,
        "StatusBar",
        state.palette.muted(),
    );
}

fn render_inline_slot(
    frame: &mut Frame<'_>,
    state: &ScreenState,
    area: Rect,
    view: &View,
    slot: Slot,
    name: &'static str,
    style: Style,
) {
    let value = view
        .slots
        .get(&slot)
        .map_or_else(|| "(not supplied)".to_owned(), |value| plain_slot(value));
    let mut lines = value.lines();
    let first = lines.next().unwrap_or_default();
    let mut rendered = vec![Line::from(vec![
        Span::styled(format!("{name}  "), state.palette.muted()),
        Span::styled(first.to_owned(), style),
    ])];
    rendered.extend(lines.map(|line| Line::styled(line.to_owned(), style)));
    frame.render_widget(Paragraph::new(rendered).wrap(Wrap { trim: false }), area);
}

fn render_state(frame: &mut Frame<'_>, state: &ScreenState, area: Rect, view: &View) {
    let value = view
        .slots
        .get(&Slot::State)
        .map_or_else(|| "(not supplied)".to_owned(), |value| plain_slot(value));
    frame.render_widget(
        Paragraph::new(value)
            .wrap(Wrap { trim: false })
            .scroll((state.program_scroll, 0)),
        area,
    );
}

fn render_inspector(frame: &mut Frame<'_>, state: &ScreenState, area: Rect) {
    let modal = centered(
        area,
        area.width.saturating_sub(6).min(120),
        area.height.saturating_sub(3),
    );
    frame.render_widget(Clear, modal);
    let host_names = state.scoped_host_names();
    let host = selected_host(state, &host_names);
    let mut lines = Vec::new();
    if let Some(snapshot) = selected_snapshot(state, host) {
        lines.push(Line::from(vec![
            Span::styled("Source ", state.palette.muted()),
            Span::raw("guest-owned program output"),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Host ", state.palette.muted()),
            Span::styled(snapshot.host.to_string(), state.palette.strong()),
            Span::styled("    exact step ", state.palette.muted()),
            Span::raw(snapshot.step.to_string()),
        ]));
        for (slot, label) in [
            (Slot::Header, "Header"),
            (Slot::Agents, "Agents"),
            (Slot::State, "State"),
            (Slot::StatusBar, "StatusBar"),
        ] {
            lines.push(Line::styled(label, state.palette.emphasis()));
            let value = snapshot
                .view
                .slots
                .get(&slot)
                .map_or_else(|| "(not supplied)".to_owned(), |value| plain_slot(value));
            lines.extend(
                value
                    .lines()
                    .map(|line| Line::styled(format!("  {line}"), state.palette.muted())),
            );
        }
        lines.push(Line::styled(
            format!("history {}", view_history_label(state)),
            state.palette.muted(),
        ));
    } else {
        lines.push(Line::styled(
            "Waiting for the program view",
            state.palette.muted(),
        ));
    }
    if let Some(outcome) = &state.outcome {
        lines.push(Line::styled("PROGRAM OUTCOME", state.palette.emphasis()));
        lines.push(Line::styled(
            format!("  {}", crate::ui::compact_json(outcome)),
            state.palette.muted(),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((state.program_scroll, 0))
            .block(panel(" PROGRAM INSPECTOR  Esc close ", state, true)),
        modal,
    );
}
