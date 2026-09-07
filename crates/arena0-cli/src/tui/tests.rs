use super::*;
use arena0_client::api::{
    ExecStatusState, HostInfo, PrivateEffectSummary, PrivateEventKind, SessionStatus,
};
use std::time::Duration;

fn config() -> TuiConfig {
    TuiConfig {
        program: "rock-paper-scissors".to_owned(),
        hosts: vec![
            TuiHost {
                host: first_host(),
                peer_id: PeerId([1; 32]),
                driver: TuiDriver::Human,
            },
            TuiHost {
                host: "host-02".parse().unwrap(),
                peer_id: PeerId([2; 32]),
                driver: TuiDriver::Builtin("sample".to_owned()),
            },
        ],
        message_schema: None,
    }
}

fn first_host() -> HostName {
    "host-01".parse().unwrap()
}

fn event_host(id: &str) -> HostInfo {
    HostInfo {
        id: id.to_owned(),
        peer_id: PeerId([0; 32]),
        user_agent: None,
    }
}

fn active_status() -> ExecStatus {
    ExecStatus {
        exec_id: arena0_client::protocol::ExecId([0x11; 32]),
        negotiation_id: Some(arena0_client::protocol::NegotiationId([0x22; 32])),
        program_id: arena0_client::protocol::ProgramHash([0x33; 32]),
        state: ExecStatusState::Active {
            session: SessionStatus {
                session_id: SessionHash([0x44; 32]),
                step: 3,
                peers: vec![PeerId([0x55; 32]), PeerId([0x66; 32])],
                participants: 3,
                pending_callout: None,
                receipt_available: false,
            },
        },
    }
}

#[test]
fn negotiation_progress_uses_only_the_selected_host_events() {
    let mut state = ScreenState::new(config());
    let mut status = active_status();
    status.state = ExecStatusState::Negotiating {
        queue_position: Some(2),
    };
    let exec_id = status.exec_id;
    state.apply(RunUpdate::Status {
        host: first_host(),
        status,
    });
    state.system_events.push(
        EventFrame::new(
            event_host("other"),
            "boot",
            3,
            3,
            EventData::NegotiationTicketAccepted {
                participant: PeerId([0xaa; 32]),
                ticket_hash: TicketHash([0xcc; 32]),
                ticket_count: 2,
                target_size: 2,
            },
            Some(exec_id),
            None,
        )
        .expect("valid event"),
    );
    state.system_events.push(
        EventFrame::new(
            event_host("host-01"),
            "boot",
            4,
            4,
            EventData::NegotiationStarted { target_size: 2 },
            Some(exec_id),
            None,
        )
        .expect("valid event"),
    );
    assert_eq!(negotiation::latest_ticket_progress(&state), Some((0, 2)));
}

fn inspection(host_number: u8, sequence: u64) -> ExecutionInspection {
    let mut status = active_status();
    status.exec_id = arena0_client::protocol::ExecId([host_number; 32]);
    ExecutionInspection {
        status,
        activation: None,
        private_from: 0,
        private: vec![PrivateCommitSummary {
            sequence,
            public_position: 2,
            event: PrivateEventKind::InputReceived,
            input_payload_bytes: Some(12),
            effects: vec![PrivateEffectSummary {
                kind: PrivateEffectKind::Broadcast,
                payload_bytes: Some(8),
            }],
            fuel_used: 7,
        }],
        private_total: 1,
        private_next: None,
    }
}

#[test]
fn empty_monitor_discovers_participants_and_renders_their_views() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut initial = config();
    initial.hosts.clear();
    let mut state = ScreenState::new_monitor(initial, actions);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    let screen = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    assert!(screen(&terminal).contains("Waiting for Participants"));

    state.apply(RunUpdate::Monitor(MonitorUpdate::Hosts {
        hosts: config()
            .hosts
            .into_iter()
            .map(|host| MonitorHost {
                host: host.host,
                peer_id: host.peer_id,
            })
            .collect(),
    }));
    for index in [1, 2] {
        let inspected = inspection(index, 0);
        let status = inspected.status.clone();
        state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
            key: MonitorExecutionKey {
                host: format!("host-{index:02}").parse().unwrap(),
                exec_id: status.exec_id,
            },
            status,
            inspection: Some(inspected),
            view: Some((3, View::new().state("Current board"))),
            trace: Vec::new(),
            observed_at: epoch_seconds(),
            stale: false,
            gap: None,
        }));
    }
    assert_eq!(state.monitor_projection().unwrap().config.hosts.len(), 2);
    terminal.draw(|frame| render(frame, &state)).unwrap();
    let output = screen(&terminal);
    assert!(output.contains("Current board"), "{output}");
}

#[test]
fn one_terminal_host_does_not_override_another_hosts_lifecycle() {
    let mut state = ScreenState::new(config());
    let mut completed = active_status();
    let ExecStatusState::Active { session } = completed.state.clone() else {
        unreachable!();
    };
    completed.state = ExecStatusState::Completed { session };
    state.apply(RunUpdate::Status {
        host: first_host(),
        status: completed,
    });
    state.apply(RunUpdate::Status {
        host: "host-02".parse().unwrap(),
        status: active_status(),
    });

    state.host_scope = HostScope::One("host-02".parse().unwrap());
    state.reconcile_scope_selection();

    assert_eq!(state.lifecycle(), ExecLifecycle::Active);
    assert_eq!(state.terminal_lifecycle, None);
}

#[test]
fn scoped_status_treats_step_divergence_and_missing_hosts_consistently() {
    let mut state = ScreenState::new(config());
    state.apply(RunUpdate::Status {
        host: first_host(),
        status: active_status(),
    });
    assert_eq!(state.scoped_status_label(), "waiting 1/2 Hosts");

    let mut later = active_status();
    let ExecStatusState::Active { session } = &mut later.state else {
        unreachable!();
    };
    session.step = 4;
    state.apply(RunUpdate::Status {
        host: "host-02".parse().unwrap(),
        status: later,
    });
    assert_eq!(state.scoped_run_state(), ScopedRunState::Mixed);
    assert_eq!(state.scoped_status_label(), "mixed");
    assert_eq!(state.scoped_status_label(), "mixed");
}

#[test]
fn failure_update_enters_a_redacted_terminal_state() {
    let mut state = ScreenState::new(config());

    state.apply(RunUpdate::Failed {
        summary: "run failed; owned executions were stopped".to_owned(),
    });

    assert!(state.complete);
    assert_eq!(state.lifecycle(), ExecLifecycle::Failed);
    assert_eq!(
        state.failure.as_deref(),
        Some("run failed; owned executions were stopped")
    );
    assert!(state.callouts.is_empty());
    assert_eq!(
        state.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(UiExit::Completed)
    );
}

#[test]
fn stop_update_enters_a_redacted_terminal_state() {
    let mut state = ScreenState::new(config());

    state.apply(RunUpdate::Stopped {
        summary: "run stopped; Host receipts verified".to_owned(),
    });

    assert!(state.complete);
    assert_eq!(state.lifecycle(), ExecLifecycle::Aborted);
    assert_eq!(
        state.failure.as_deref(),
        Some("run stopped; Host receipts verified")
    );
    assert!(state.callouts.is_empty());
}

#[test]
fn input_keys_submit_json_and_escape_clears() {
    let mut state = ScreenState::new(config());
    let (reply, answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "Choose".to_owned(),
        prompt: "Choose".to_owned(),
        context: Value::Null,
        schema: serde_json::json!({"type": "object"}),
        reply,
    });
    state.set_answer_text("discard");
    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.answer_text(), "discard");
    assert_eq!(state.focus, Focus::Hosts);
    state.set_answer_text("{\"move\":1}");
    state.focus = Focus::Composer;
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        answer.blocking_recv().unwrap(),
        serde_json::json!({"move": 1})
    );
}

#[test]
fn invalid_tui_answer_keeps_the_callout_until_valid_input() {
    let mut state = ScreenState::new(config());
    let (reply, answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "Choose".to_owned(),
        prompt: "Cooperate or defect".to_owned(),
        context: Value::Null,
        schema: serde_json::json!({"enum": ["Cooperate", "Defect"]}),
        reply,
    });
    assert_eq!(state.focus, Focus::Composer);

    state.set_answer_text("cooperate");
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!state.callouts.is_empty());
    assert!(state.answer_validation_error().is_some());

    state.set_answer_text("Cooperate");
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(state.callouts.is_empty());
    assert_eq!(state.focus, Focus::Workspace);
    assert_eq!(
        answer.blocking_recv().unwrap(),
        serde_json::json!("Cooperate")
    );
}

#[test]
fn quit_and_ctrl_c_cancel() {
    let mut state = ScreenState::new(config());
    assert_eq!(
        state.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(UiExit::Cancelled)
    );
    assert_eq!(
        state.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(UiExit::Cancelled)
    );
}

#[test]
fn duplicate_callout_key_does_not_replace_pending_reply() {
    let mut state = ScreenState::new(config());
    let (first, _first_answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "first".to_owned(),
        prompt: "first".to_owned(),
        context: Value::Null,
        schema: serde_json::json!(true),
        reply: first,
    });
    let (second, second_answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "second".to_owned(),
        prompt: "second".to_owned(),
        context: Value::Null,
        schema: serde_json::json!(true),
        reply: second,
    });
    assert_eq!(
        state
            .callouts
            .selected()
            .map(|callout| callout.name.as_str()),
        Some("first")
    );
    assert!(second_answer.blocking_recv().is_err());
}

#[test]
fn host_callouts_keep_independent_textarea_drafts_and_reply_routes() {
    let mut state = ScreenState::new(config());
    let (first_reply, first_answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "first".to_owned(),
        prompt: "first".to_owned(),
        context: Value::Null,
        schema: serde_json::json!({"type": "string"}),
        reply: first_reply,
    });
    let second_host: HostName = "host-02".parse().unwrap();
    let (second_reply, mut second_answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: second_host.clone(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(2),
        callout_index: 1,
        name: "second".to_owned(),
        prompt: "second".to_owned(),
        context: Value::Null,
        schema: serde_json::json!({"type": "string"}),
        reply: second_reply,
    });

    state.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL));
    assert_eq!(
        state.callouts.selected().map(|callout| callout.scroll),
        Some(4)
    );
    state.set_answer_text("answer from host-01");
    state.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
    assert_eq!(
        state.callouts.selected().map(|callout| &callout.host),
        Some(&second_host)
    );
    assert_eq!(
        state.callouts.selected().map(|callout| callout.scroll),
        Some(0)
    );
    assert_eq!(state.callouts.position(), 2);
    state.set_answer_text("answer from host-02");
    state.on_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
    assert_eq!(state.answer_text(), "answer from host-01");
    assert_eq!(
        state.callouts.selected().map(|callout| callout.scroll),
        Some(4)
    );

    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        first_answer.blocking_recv().unwrap(),
        serde_json::json!("answer from host-01")
    );
    assert!(second_answer.try_recv().is_err());
    assert_eq!(state.answer_text(), "answer from host-02");
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        second_answer.blocking_recv().unwrap(),
        serde_json::json!("answer from host-02")
    );
}

#[test]
fn overview_focus_drills_into_tabs_and_escape_returns() {
    let mut state = ScreenState::new(config());
    assert_eq!(state.page.view(), WorkspaceView::Overview);
    assert_eq!(state.page.pane(), OverviewPane::Program);
    assert_eq!(state.focus, Focus::Hosts);

    state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Overview);
    assert_eq!(state.focus, Focus::Workspace);
    assert_eq!(state.page.pane(), OverviewPane::Program);
    state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.page.pane(), OverviewPane::PublicTrace);
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::PublicTrace);
    state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::PublicTrace);
    state.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Program);
    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Overview);
    assert_eq!(state.page.pane(), OverviewPane::Program);
}

#[test]
fn detail_enter_opens_inspector_and_escape_unwinds_to_overview() {
    let mut state = ScreenState::new(config());
    state.page.select(WorkspaceView::PublicTrace);
    state.focus = Focus::Workspace;

    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(state.page.inspector(), Some(OverviewPane::PublicTrace));

    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.page.inspector(), None);
    assert_eq!(state.page.view(), WorkspaceView::PublicTrace);

    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Overview);
}

#[test]
fn pending_answer_can_be_left_and_reentered() {
    let mut state = ScreenState::new(config());
    let (reply, _answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "Choose".to_owned(),
        prompt: "Choose".to_owned(),
        context: Value::Null,
        schema: serde_json::json!(true),
        reply,
    });

    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.focus, Focus::Hosts);
    assert!(!state.callouts.is_empty());

    state.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    assert_eq!(state.focus, Focus::Composer);
    assert!(!state.callouts.is_empty());
}

#[test]
fn tab_moves_focus_inside_the_current_view_only() {
    let mut state = ScreenState::new(config());
    let (reply, _answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "Choose".to_owned(),
        prompt: "Choose".to_owned(),
        context: Value::Null,
        schema: serde_json::json!(true),
        reply,
    });

    state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.focus, Focus::Hosts);
    state.on_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Wasm);

    state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Wasm);
    assert_eq!(state.page.inspector(), Some(OverviewPane::Wasm));
    assert_eq!(state.page.region(), DetailRegion::Inspector);
    assert_eq!(state.focus, Focus::Workspace);
    state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Wasm);
    assert_eq!(state.page.inspector(), None);
    assert_eq!(state.page.region(), DetailRegion::Records);
    assert_eq!(state.focus, Focus::Composer);
    state.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(state.page.view(), WorkspaceView::Wasm);
    assert_eq!(state.focus, Focus::Workspace);
}

#[test]
fn composer_shortcut_then_backtab_returns_to_visible_records() {
    let mut state = ScreenState::new(config());
    let (reply, _answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "Choose".to_owned(),
        prompt: "Choose".to_owned(),
        context: Value::Null,
        schema: serde_json::json!(true),
        reply,
    });
    for code in [
        KeyCode::Esc,
        KeyCode::Char('4'),
        KeyCode::Enter,
        KeyCode::Char('i'),
        KeyCode::BackTab,
    ] {
        state.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    assert_eq!(state.page.view(), WorkspaceView::PublicTrace);
    assert_eq!(state.page.region(), DetailRegion::Records);
    assert_eq!(state.page.inspector(), None);
    assert_eq!(state.focus, Focus::Workspace);
    state.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.details_scroll, 0);
}

#[test]
fn arrivals_preserve_callout_order_and_submission_selects_the_oldest() {
    let mut state = ScreenState::new(config());
    let mut answers = Vec::new();
    for index in 0..3 {
        let (reply, answer) = oneshot::channel();
        answers.push(answer);
        state.apply(RunUpdate::Callout {
            host: HostName::for_local_index(index),
            exec_id: active_status().exec_id,
            pending_id: PendingId::new(1),
            callout_index: 1,
            name: "Choose".to_owned(),
            prompt: "Choose".to_owned(),
            context: Value::Null,
            schema: serde_json::json!({"type": "string"}),
            reply,
        });
        if index == 1 {
            state.set_answer_text("first draft");
            state.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
            state.set_answer_text("second draft");
        }
    }
    assert_eq!(state.callouts.position(), 2);
    assert_eq!(state.answer_text(), "second draft");
    state.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
    assert_eq!(state.callouts.position(), 3);
    state.set_answer_text("third answer");
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        answers[2].try_recv().unwrap(),
        serde_json::json!("third answer")
    );
    assert!(answers[0].try_recv().is_err());
    assert!(answers[1].try_recv().is_err());
    assert_eq!(state.callouts.position(), 1);
    assert_eq!(state.answer_text(), "first draft");
    state.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
    assert_eq!(state.answer_text(), "second draft");
}

#[test]
fn detail_region_routes_program_hosts_and_inspector_scrolling() {
    let mut state = ScreenState::new(config());
    state.page.select(WorkspaceView::Program);
    state.focus = Focus::Workspace;
    state.host_scope = HostScope::All;
    assert_eq!(state.selected_host, Some(first_host()));

    state.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.selected_host, Some("host-02".parse().unwrap()));

    state.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(state.page.region(), DetailRegion::Inspector);
    assert_eq!(state.program_scroll, 8);

    state.page.select(WorkspaceView::Negotiation);
    state.page.open_inspector();
    state.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(state.details_scroll, 8);

    state.on_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(state.page.inspector(), Some(OverviewPane::PublicTrace));
    assert_eq!(state.details_scroll, 0);
}

#[test]
fn layout_pins_composer_to_the_bottom() {
    let area = Rect::new(0, 0, 100, 30);
    let layout = screen_layout(area, 9);
    assert_eq!(
        layout.composer.y + layout.composer.height,
        area.y + area.height
    );
    assert!(layout.masthead.y + layout.masthead.height <= layout.tabs.y);
    assert!(layout.tabs.y + layout.tabs.height <= layout.workspace.y);
    assert_eq!(
        layout.workspace.y + layout.workspace.height,
        layout.composer.y + 1
    );
    assert!(layout.hosts.x < layout.workspace.x);
    assert_eq!(layout.hosts.y, layout.workspace.y);
    assert_eq!(layout.hosts.y + layout.hosts.height, area.y + area.height);

    assert!(layout.workspace.height >= 7);
}

#[test]
fn shell_keeps_hosts_persistent_and_gives_narrow_input_the_full_width() {
    let wide = Rect::new(0, 0, 160, 48);
    let wide_layout = screen_layout(wide, 7);
    assert_eq!(wide_layout.hosts.width, 28);
    assert!(wide_layout.hosts.x < wide_layout.workspace.x);
    assert!(wide_layout.hosts.x < wide_layout.composer.x);
    assert_eq!(wide_layout.workspace.x, wide_layout.composer.x);

    let narrow = Rect::new(0, 0, 80, 30);
    let narrow_layout = screen_layout(narrow, 7);
    assert!(narrow_layout.hosts.x < narrow_layout.workspace.x);
    assert_eq!(narrow_layout.composer.x, narrow.x);
    assert_eq!(narrow_layout.composer.width, narrow.width);
}

#[test]
fn hosts_have_an_independent_cursor_and_enter_activates_scope() {
    let mut state = ScreenState::new(config());
    assert_eq!(state.focus, Focus::Hosts);
    assert_eq!(state.host_cursor, 0);
    assert!(matches!(state.host_scope, HostScope::All));

    state.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.host_cursor, 1);
    assert!(matches!(state.host_scope, HostScope::All));
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(state.host_scope, HostScope::One(first_host()));

    state.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(state.host_scope, HostScope::All));
}

#[test]
fn program_view_requests_the_width_of_its_actual_projection() {
    let area = Rect::new(0, 0, 120, 40);
    let mut state = ScreenState::new(config());
    assert_eq!(program_view_width(&state, area), 93);

    state.page.select(WorkspaceView::Program);
    state.host_scope = HostScope::One(first_host());
    assert_eq!(program_view_width(&state, area), 93);

    state.host_scope = HostScope::Compare {
        left: first_host(),
        right: "host-02".parse().unwrap(),
    };
    assert_eq!(program_view_width(&state, area), 46);
}

#[test]
fn overview_uses_a_wide_mosaic_and_preserves_program_content_height() {
    let mut state = ScreenState::new(config());
    state.apply(RunUpdate::View {
        host: first_host(),
        step: 1,
        view: View::new().state("rank 8\nrank 7\nrank 6\nrank 5"),
    });
    let area = Rect::new(0, 0, 100, 30);
    let layout = overview_layout(&state, area);

    assert_eq!(layout.program.y, layout.negotiation.y);
    assert!(layout.program.x < layout.negotiation.x);
    assert!(layout.public_trace.y > layout.program.y);
    assert_eq!(layout.public_trace.y, layout.wasm.y);
    assert!(layout.public_trace.x < layout.wasm.x);
    assert!(layout.system_events.y > layout.public_trace.y);
    assert!(layout.program.height >= overview_program_height(&state, layout.program.width));

    let minimum = overview_layout(&state, Rect::new(0, 0, 100, MIN_OVERVIEW_HEIGHT));
    for pane in [
        minimum.program,
        minimum.public_trace,
        minimum.negotiation,
        minimum.wasm,
        minimum.system_events,
    ] {
        assert!(pane.height >= 3);
    }
}

#[test]
fn compact_overview_retains_every_pane_region() {
    let state = ScreenState::new(config());
    let layout = overview_layout(&state, Rect::new(0, 0, 60, 8));
    for pane in [
        layout.program,
        layout.public_trace,
        layout.negotiation,
        layout.wasm,
        layout.system_events,
    ] {
        assert!(pane.height >= 2);
    }
}

#[test]
fn all_hosts_program_summary_reports_incomplete_uniform_and_different_views() {
    let mut state = ScreenState::new(config());
    state.apply(RunUpdate::View {
        host: first_host(),
        step: 2,
        view: View::new().state("same state"),
    });
    let (title, _, _) = overview_program_content(&state);
    assert!(title.contains("INCOMPLETE 1/2"));

    state.apply(RunUpdate::View {
        host: "host-02".parse().unwrap(),
        step: 2,
        view: View::new().state("same state"),
    });
    let (title, body, _) = overview_program_content(&state);
    assert!(title.contains("ALL 2 SAME"));
    assert_eq!(body, "same state");

    state.apply(RunUpdate::View {
        host: "host-02".parse().unwrap(),
        step: 2,
        view: View::new().state("different state"),
    });
    let (title, body, _) = overview_program_content(&state);
    assert!(title.contains("DIFFERENT"));
    assert!(body.contains("host-01"));
    assert!(body.contains("host-02"));
}

#[test]
fn view_history_is_bounded_and_navigable() {
    let mut state = ScreenState::new(config());
    for step in 0..(MAX_VIEW_HISTORY as u64 + 4) {
        state.apply(RunUpdate::View {
            host: first_host(),
            step,
            view: View::new().state(format!("score {step}")),
        });
    }

    let history = state.view_history.get(&first_host()).unwrap();
    assert_eq!(history.len(), MAX_VIEW_HISTORY);
    assert_eq!(history.front().map(|snapshot| snapshot.step), Some(4));
    assert_eq!(history.back().map(|snapshot| snapshot.step), Some(259));
    assert!(state.is_live_view());

    state.page.select(WorkspaceView::Program);
    state.focus = Focus::Workspace;
    state.on_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert!(!state.is_live_view());
    assert_eq!(
        state
            .active_view()
            .and_then(|view| view.slots.get(&Slot::State)),
        Some(&"score 258".to_owned())
    );
    state.apply(RunUpdate::View {
        host: first_host(),
        step: 259,
        view: View::new().state("score 259 resized"),
    });
    assert_eq!(state.view_new_count, 0);
    assert_eq!(
        state
            .active_view()
            .and_then(|view| view.slots.get(&Slot::State)),
        Some(&"score 258".to_owned())
    );
    state.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    assert!(state.is_live_view());
    state.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert!(state.is_live_view());
}

#[test]
fn program_history_is_bounded_independently_for_each_host() {
    let mut state = ScreenState::new(config());
    let other: HostName = "host-02".parse().unwrap();
    for step in 0..(MAX_VIEW_HISTORY as u64 + 8) {
        for host in [first_host(), other.clone()] {
            state.apply(RunUpdate::View {
                host,
                step,
                view: View::new().state(format!("state {step}")),
            });
        }
    }

    assert_eq!(
        state.view_history.get(&first_host()).unwrap().len(),
        MAX_VIEW_HISTORY
    );
    assert_eq!(
        state.view_history.get(&other).unwrap().len(),
        MAX_VIEW_HISTORY
    );
    assert_eq!(
        state
            .view_history
            .get(&first_host())
            .unwrap()
            .front()
            .unwrap()
            .step,
        8
    );
    assert_eq!(
        state
            .view_history
            .get(&other)
            .unwrap()
            .front()
            .unwrap()
            .step,
        8
    );
}

#[test]
fn compare_scope_keeps_distinct_hosts_and_reconciles_hidden_events() {
    let mut state = ScreenState::new(config());
    state.compare_hosts();
    let HostScope::Compare { left, right } = &state.host_scope else {
        panic!("compare scope changed kind");
    };
    assert_ne!(left, right);

    let exec_id = active_status().exec_id;
    for (host, seq) in [("host-01", 1), ("host-02", 2)] {
        state.system_events.push(
            EventFrame::new(
                event_host(host),
                "boot",
                seq,
                seq,
                EventData::NegotiationPeers {
                    lifecycle: ExecLifecycle::Negotiating,
                    peers: Vec::new(),
                },
                Some(exec_id),
                None,
            )
            .unwrap(),
        );
    }
    state.selected_event = events::keys(&state).last().cloned();
    state.host_scope = HostScope::One(first_host());
    state.reconcile_scope_selection();
    assert_eq!(
        state.selected_event.as_ref().map(|key| key.host.as_str()),
        Some("host-01")
    );
}

#[test]
fn same_step_view_refresh_replaces_the_live_snapshot() {
    let mut state = ScreenState::new(config());
    state.apply(RunUpdate::View {
        host: first_host(),
        step: 4,
        view: View::new().state("before resize"),
    });
    state.apply(RunUpdate::View {
        host: first_host(),
        step: 4,
        view: View::new().state("after resize"),
    });

    let history = state.view_history.get(&first_host()).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].step, 4);
    assert_eq!(
        state
            .active_view()
            .and_then(|view| view.slots.get(&Slot::State)),
        Some(&"after resize".to_owned())
    );
}

#[test]
fn private_crossings_are_aggregated_by_host_without_payloads() {
    let mut state = ScreenState::new(config());
    state.apply(RunUpdate::Inspection {
        host: "host-01".parse().expect("valid Host name"),
        inspection: Box::new(inspection(1, 3)),
    });
    state.apply(RunUpdate::Inspection {
        host: "host-02".parse().expect("valid Host name"),
        inspection: Box::new(inspection(2, 5)),
    });

    assert_eq!(state.inspections.len(), 2);
    assert_eq!(
        state
            .inspections
            .values()
            .map(|inspection| inspection.private.len())
            .sum::<usize>(),
        2
    );
    assert_eq!(
        state
            .inspections
            .values()
            .map(|inspection| inspection.private_total)
            .sum::<u64>(),
        2
    );
}

#[test]
fn wasm_page_keys_request_the_selected_hosts_adjacent_private_page() {
    let host = first_host();
    let mut state = ScreenState::new(config());
    let mut page = inspection(1, 300);
    page.private_from = 256;
    page.private_total = 700;
    page.private_next = Some(512);
    state.apply(RunUpdate::Inspection {
        host: host.clone(),
        inspection: Box::new(page),
    });
    state.page.select(WorkspaceView::Wasm);

    state.on_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE));
    assert_eq!(
        state.private_page_request.take(),
        Some(PrivatePageRequest {
            host: host.clone(),
            from: Some(0),
        })
    );

    state.on_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE));
    assert_eq!(
        state.private_page_request.take(),
        Some(PrivatePageRequest {
            host,
            from: Some(512),
        })
    );
}

#[test]
fn private_crossings_use_the_preceding_public_step_without_inventing_one() {
    assert_eq!(wasm::private_parent_step(0), None);
    assert_eq!(wasm::private_parent_step(1), Some(0));
    assert_eq!(wasm::private_parent_step(2), Some(1));

    let mut state = ScreenState::new(config());
    state.apply(RunUpdate::Inspection {
        host: "host-01".parse().expect("valid Host name"),
        inspection: Box::new(inspection(1, 3)),
    });

    assert_eq!(
        wasm::keys(&state),
        vec![
            CrossingKey::Boundary { after_position: 2 },
            CrossingKey::Private {
                host: "host-01".parse().expect("valid Host name"),
                sequence: 3,
            },
        ]
    );
    assert!(!wasm::keys(&state).contains(&CrossingKey::Public { step: 2 }));
}

#[test]
fn composer_height_tracks_wrapped_semantic_rows_and_editor_lines() {
    let mut state = ScreenState::new(config());
    assert_eq!(composer_height(&state, 80), 2);
    let (reply, _answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "Choose".to_owned(),
        prompt: "x".repeat(300),
        context: serde_json::json!({"detail": "y".repeat(300)}),
        schema: serde_json::json!({"enum": ["Cooperate", "Defect"]}),
        reply,
    });
    let focused_height = composer_height(&state, 80);
    assert!(focused_height > 8);
    state.focus = Focus::Workspace;
    assert_eq!(composer_height(&state, 80), focused_height);
    assert!(composer_height(&state, 20) > focused_height);

    state.callouts.selected_mut().unwrap().validation_error = Some("try again".to_owned());
    assert_eq!(composer_height(&state, 80), focused_height + 1);

    let area = Rect::new(0, 0, 100, focused_height.saturating_add(20));
    let layout = screen_layout(area, focused_height);
    assert_eq!(layout.composer.height, focused_height);

    let minimum_terminal = Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT);
    let required = composer_height(&state, minimum_terminal.width);
    let constrained = screen_layout(minimum_terminal, required);
    assert!(composer_overflows(
        &state,
        Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT.saturating_sub(2))
    ));
    assert_eq!(constrained.composer.height, MIN_HEIGHT.saturating_sub(2));
    assert_eq!(
        constrained.composer.y + constrained.composer.height,
        minimum_terminal.y + minimum_terminal.height
    );
}

#[test]
fn host_qualified_views_preserve_each_host_history() {
    let mut state = ScreenState::new(config());
    let other: HostName = "host-02".parse().expect("valid Host name");
    state.apply(RunUpdate::View {
        host: first_host(),
        step: 1,
        view: View::new().state("host-01 state"),
    });
    state.apply(RunUpdate::View {
        host: other.clone(),
        step: 1,
        view: View::new().state("other state"),
    });
    assert_eq!(state.view_history.len(), 2);
    assert_eq!(
        state
            .active_view()
            .and_then(|view| view.slots.get(&Slot::State)),
        Some(&"host-01 state".to_owned())
    );
    state.select_host(other);
    assert_eq!(
        state
            .active_view()
            .and_then(|view| view.slots.get(&Slot::State)),
        Some(&"other state".to_owned())
    );
}

#[test]
fn insert_mode_keeps_printable_controls_and_supports_editing() {
    let mut state = ScreenState::new(config());
    let (reply, _answer) = oneshot::channel();
    state.apply(RunUpdate::Callout {
        host: first_host(),
        exec_id: active_status().exec_id,
        pending_id: PendingId::new(1),
        callout_index: 1,
        name: "Choose".to_owned(),
        prompt: "value".to_owned(),
        context: Value::Null,
        schema: serde_json::json!(true),
        reply,
    });
    assert_eq!(
        state.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        state.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
        None
    );
    state.on_paste("abc".to_owned());
    state.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(state.answer_text(), "?abc");
    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.answer_text(), "?abc");
    assert_eq!(state.focus, Focus::Hosts);
}

#[test]
fn receipt_evidence_is_host_owned_and_progress_is_aggregate() {
    let mut state = ScreenState::new(config());
    state.apply(RunUpdate::ReceiptVerified {
        host: first_host(),
        peer_id: PeerId([1; 32]),
        tier: "peer_id",
    });
    state.apply(RunUpdate::ReceiptVerified {
        host: "host-02".parse().expect("valid Host name"),
        peer_id: PeerId([2; 32]),
        tier: "peer_id",
    });
    state.apply(RunUpdate::VerificationProgress {
        verified: 2,
        total: 2,
        tier: "aggregate",
    });
    assert_eq!(state.receipts.len(), 2);
    assert_eq!(state.receipts[0].host, first_host());
    assert_eq!(state.verification_progress, Some((2, 2, "aggregate")));
}

#[test]
fn monitor_keeps_same_host_executions_separate() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    let mut first = active_status();
    first.exec_id = ExecId([1; 32]);
    let mut second = active_status();
    second.exec_id = ExecId([2; 32]);
    let host = first_host();
    for status in [first.clone(), second.clone()] {
        state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
            key: MonitorExecutionKey {
                host: host.clone(),
                exec_id: status.exec_id,
            },
            status,
            message_schema: None,
            inspection: None,
            view: None,
            trace: Vec::new(),
            observed_at: 7,
            stale: false,
            gap: None,
        }));
    }
    let monitor = state.monitor.as_ref().expect("monitor state");
    assert_eq!(monitor.executions.len(), 2);
    assert_eq!(
        monitor.selected.as_ref().map(|key| key.exec_id),
        Some(ExecId([1; 32]))
    );
    assert_eq!(
        monitor.executions[&MonitorExecutionKey {
            host: host.clone(),
            exec_id: ExecId([1; 32])
        }]
            .status
            .exec_id,
        ExecId([1; 32])
    );
    assert_eq!(
        monitor.executions[&MonitorExecutionKey {
            host,
            exec_id: ExecId([2; 32])
        }]
            .status
            .exec_id,
        ExecId([2; 32])
    );
}

#[test]
fn monitor_a_opens_only_the_selected_execution_callout() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    let host = first_host();
    let mut first = active_status();
    first.exec_id = ExecId([1; 32]);
    let mut second = active_status();
    second.exec_id = ExecId([2; 32]);
    for status in [first.clone(), second.clone()] {
        state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
            key: MonitorExecutionKey {
                host: host.clone(),
                exec_id: status.exec_id,
            },
            status,
            message_schema: None,
            inspection: None,
            view: None,
            trace: Vec::new(),
            observed_at: 1,
            stale: false,
            gap: None,
        }));
    }
    for exec_id in [first.exec_id, second.exec_id] {
        state.apply(RunUpdate::Monitor(MonitorUpdate::Callout {
            host: host.clone(),
            exec_id,
            pending_id: PendingId::new(exec_id.0[0] as u64),
            callout_index: 1,
            name: "Choose".to_owned(),
            prompt: "choose".to_owned(),
            context: Value::Null,
            schema: serde_json::json!({"type": "string"}),
        }));
    }
    state.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    let first_callout = state.callouts.selected().expect("first callout");
    assert_eq!(state.focus, Focus::Composer);
    assert_eq!(first_callout.exec_id, first.exec_id);
    state.focus = Focus::Hosts;
    state
        .monitor
        .as_mut()
        .expect("monitor")
        .move_cursor(1, true);
    state.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    let second_callout = state.callouts.selected().expect("second callout");
    assert_eq!(second_callout.exec_id, second.exec_id);
}

#[test]
fn monitor_acceptance_removes_only_the_answered_key() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    let host = first_host();
    for byte in [1_u8, 2] {
        state.apply(RunUpdate::Monitor(MonitorUpdate::Callout {
            host: host.clone(),
            exec_id: ExecId([byte; 32]),
            pending_id: PendingId::new(u64::from(byte)),
            callout_index: 1,
            name: "Choose".to_owned(),
            prompt: "choose".to_owned(),
            context: Value::Null,
            schema: serde_json::json!({"type": "string"}),
        }));
    }
    state.apply(RunUpdate::Monitor(MonitorUpdate::Submission {
        host: host.clone(),
        exec_id: ExecId([1; 32]),
        pending_id: PendingId::new(1),
        result: MonitorSubmission::Accepted,
    }));
    assert_eq!(state.callouts.len(), 1);
    assert_eq!(
        state
            .callouts
            .selected()
            .expect("remaining callout")
            .exec_id,
        ExecId([2; 32])
    );
}

#[test]
fn monitor_enter_scrolls_detail_and_escape_returns_to_overview() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    let status = active_status();
    let host = first_host();
    state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
        key: MonitorExecutionKey {
            host,
            exec_id: status.exec_id,
        },
        status,
        message_schema: None,
        inspection: None,
        view: None,
        trace: Vec::new(),
        observed_at: 1,
        stale: false,
        gap: None,
    }));

    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(state.focus, Focus::Workspace);
    assert!(state.monitor.as_ref().expect("monitor").detail);
    assert_eq!(
        state.monitor.as_ref().expect("monitor").view,
        WorkspaceView::Overview
    );
    state.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(state.page.pane(), OverviewPane::Program.next());
    assert_eq!(state.monitor_projection().unwrap().page, state.page);
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        state.monitor.as_ref().unwrap().view,
        state.page.pane().view()
    );
    state.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.details_scroll, 1);

    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let monitor = state.monitor.as_ref().expect("monitor");
    assert_eq!(state.focus, Focus::Hosts);
    assert!(!monitor.detail);
    assert_eq!(monitor.view, WorkspaceView::Overview);
}

#[test]
fn monitor_session_filter_and_freeze_keep_bounded_display_snapshot() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    for byte in [1_u8, 2, 3] {
        let mut status = active_status();
        status.exec_id = ExecId([byte; 32]);
        if byte == 3 {
            status.state = ExecStatusState::Active {
                session: SessionStatus {
                    session_id: SessionHash([0x77; 32]),
                    ..status.session().expect("active session").clone()
                },
            };
        }
        state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
            key: MonitorExecutionKey {
                host: if byte == 2 {
                    "host-02".parse().expect("valid Host")
                } else {
                    first_host()
                },
                exec_id: status.exec_id,
            },
            status,
            message_schema: None,
            inspection: None,
            view: None,
            trace: Vec::new(),
            observed_at: 1,
            stale: false,
            gap: None,
        }));
    }
    assert_eq!(
        state
            .monitor
            .as_ref()
            .expect("monitor")
            .ordered_keys()
            .len(),
        3
    );

    state.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    assert_eq!(
        state
            .monitor
            .as_ref()
            .expect("monitor")
            .ordered_keys()
            .len(),
        2
    );
    state.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(state.monitor.as_ref().expect("monitor").is_frozen());
    let frozen_count = state
        .monitor
        .as_ref()
        .expect("monitor")
        .display_executions()
        .len();
    let mut fresh = active_status();
    fresh.exec_id = ExecId([9; 32]);
    state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
        key: MonitorExecutionKey {
            host: first_host(),
            exec_id: fresh.exec_id,
        },
        status: fresh,
        message_schema: None,
        inspection: None,
        view: None,
        trace: Vec::new(),
        observed_at: 2,
        stale: false,
        gap: None,
    }));
    assert_eq!(
        state
            .monitor
            .as_ref()
            .expect("monitor")
            .display_executions()
            .len(),
        frozen_count
    );
    state.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(!state.monitor.as_ref().expect("monitor").is_frozen());
}

#[test]
fn monitor_home_then_down_moves_to_second_row_after_last_selection() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    for byte in [1_u8, 2, 3] {
        let mut status = active_status();
        status.exec_id = ExecId([byte; 32]);
        state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
            key: MonitorExecutionKey {
                host: first_host(),
                exec_id: status.exec_id,
            },
            status,
            message_schema: None,
            inspection: None,
            view: None,
            trace: Vec::new(),
            observed_at: 1,
            stale: false,
            gap: None,
        }));
    }
    state.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(
        state
            .monitor
            .as_ref()
            .expect("monitor")
            .selected
            .as_ref()
            .map(|key| key.exec_id),
        Some(ExecId([2; 32]))
    );
}

#[test]
fn monitor_trace_detail_decodes_messages_and_scrolls_inspector() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    let status = active_status();
    let exec_id = status.exec_id;
    let trace = (0..3)
        .map(|step| TraceEntry {
            trace_version: arena0_client::protocol::TRACE_FORMAT_VERSION,
            step,
            event: PublicEvent::MessageReceived {
                message_id: arena0_client::protocol::MessageId([step as u8; 32]),
                from: PeerId([1; 32]),
                position: step,
                pre_state: arena0_client::protocol::StateHash([step as u8; 32]),
                msg: (step as u32).to_le_bytes().to_vec(),
            },
            effects: Vec::new(),
            pre_state: arena0_client::protocol::StateHash([step as u8; 32]),
            post_state: arena0_client::protocol::StateHash([(step + 1) as u8; 32]),
            fuel_used: 1,
            witness: None,
            agreement: arena0_client::protocol::AggregateAttestation::empty(),
        })
        .collect();
    state.apply(RunUpdate::Monitor(MonitorUpdate::Execution {
        key: MonitorExecutionKey {
            host: first_host(),
            exec_id,
        },
        status,
        message_schema: Some(BorshSchemaDocument::for_type::<u32>()),
        inspection: None,
        view: None,
        trace,
        observed_at: 1,
        stale: false,
        gap: None,
    }));
    let projection = state.monitor_projection().expect("projection");
    let message = projection
        .host_trace(&first_host())
        .last()
        .and_then(|entry| entry.message.as_ref())
        .expect("message projection");
    assert_eq!(
        message.as_ref().expect("decoded message"),
        &serde_json::json!(2)
    );
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    state.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(
        state
            .monitor_projection()
            .expect("projection")
            .selected_public_position,
        Some(1)
    );
    state.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        state.monitor.as_ref().expect("monitor").region,
        DetailRegion::Inspector
    );
    state.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.details_scroll, 1);
    state.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        state.monitor.as_ref().expect("monitor").region,
        DetailRegion::Records
    );
}

#[test]
fn monitor_submission_routes_to_exact_callout_and_closes_selected_composer() {
    let (actions, _receiver) = mpsc::channel(4);
    let mut state = ScreenState::new_monitor(config(), actions);
    let host = first_host();
    for byte in [1_u8, 2] {
        state.apply(RunUpdate::Monitor(MonitorUpdate::Callout {
            host: host.clone(),
            exec_id: ExecId([byte; 32]),
            pending_id: PendingId::new(u64::from(byte)),
            callout_index: 1,
            name: format!("Choice-{byte}"),
            prompt: "choose".to_owned(),
            context: Value::Null,
            schema: serde_json::json!({"type": "string"}),
        }));
    }
    state.focus = Focus::Composer;
    state.apply(RunUpdate::Monitor(MonitorUpdate::Submission {
        host: host.clone(),
        exec_id: ExecId([2; 32]),
        pending_id: PendingId::new(2),
        result: MonitorSubmission::Rejected("\u{1b}[31mtransport\u{1b}[0m".to_owned()),
    }));
    assert_eq!(state.focus, Focus::Composer);
    assert_eq!(
        state
            .callouts
            .get_mut(&host, ExecId([2; 32]), PendingId::new(2))
            .expect("retained callout")
            .submission_error
            .as_deref(),
        Some("transport")
    );
    state.apply(RunUpdate::Monitor(MonitorUpdate::Submission {
        host,
        exec_id: ExecId([1; 32]),
        pending_id: PendingId::new(1),
        result: MonitorSubmission::Accepted,
    }));
    assert_eq!(state.focus, Focus::Hosts);
    assert_eq!(state.callouts.len(), 1);
}

#[tokio::test]
async fn tui_session_join_can_be_cancelled_and_retried() {
    let (updates, _receiver) = mpsc::channel(1);
    let (_width, width_receiver) = watch::channel(80);
    let (_private_page, private_receiver) = watch::channel(None);
    let (done_sender, done_receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = done_receiver.await;
        Ok(UiExit::Closed)
    });
    let mut session = TuiSession {
        handle: TuiHandle {
            updates,
            width: width_receiver,
            private_page: private_receiver,
        },
        task: Some(task),
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(1), session.wait())
            .await
            .is_err()
    );
    assert!(session.task.is_some());
    done_sender.send(()).expect("pending session is alive");
    session.wait().await.expect("second join succeeds");
    assert!(session.task.is_none());
}
