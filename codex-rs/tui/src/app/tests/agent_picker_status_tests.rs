use super::*;
use crate::app::agent_picker_status::AgentPickerStatus;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn agent_picker_collapses_unloaded_history_behind_more() {
    let mut app = make_test_app().await;
    app.agent_navigation.clear();
    for (name, turn_status, loaded) in [
        ("working", Some(TurnStatus::InProgress), true),
        ("finished", Some(TurnStatus::Completed), true),
        ("stopped", Some(TurnStatus::Interrupted), true),
        ("failed", Some(TurnStatus::Failed), true),
        ("idle", None, true),
        ("history", Some(TurnStatus::Completed), false),
        ("unobserved", None, true),
    ] {
        let thread_id = ThreadId::new();
        app.agent_navigation.upsert(
            thread_id,
            Some(name.to_string()),
            /*agent_role*/ None,
            !loaded,
        );
        if name != "unobserved" {
            let turns = turn_status
                .map(|status| test_turn("turn", status, Vec::new()))
                .into_iter()
                .collect();
            app.thread_event_channels.insert(
                thread_id,
                ThreadEventChannel::new_with_session(
                    THREAD_EVENT_CHANNEL_CAPACITY,
                    test_thread_session(thread_id, test_path_buf("/tmp/project")),
                    turns,
                ),
            );
        }
    }
    let params = app.agent_picker_selection_view_params(/*selected*/ None);
    let rows = params
        .items
        .iter()
        .map(|item| {
            let prefix: String = item
                .name_prefix_spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            format!("{prefix}{}", item.name)
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(rows, @"
    [running]     working
    [done]        finished
    [interrupted] stopped
    [error]       failed
    [idle]        idle
    [unknown]     unobserved
    more...
    ");
    assert_eq!(
        params
            .items
            .last()
            .and_then(|item| item.description.as_deref()),
        Some("1 unloaded history")
    );

    let history = app.agent_history_selection_view_params(/*selected*/ None);
    let history_rows = history
        .items
        .iter()
        .map(|item| {
            let prefix: String = item
                .name_prefix_spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            format!("{prefix}{}", item.name)
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(history.title.as_deref(), Some("Subagent history (1)"));
    assert_snapshot!(history_rows, @"[not loaded]  history");
}

#[tokio::test]
async fn agent_picker_excludes_hidden_threads_from_live_and_history_views() {
    use crate::app::agent_navigation::AgentPickerThreadVisibility;

    let mut app = make_test_app().await;
    app.agent_navigation.clear();
    for is_closed in [false, true] {
        let thread_id = ThreadId::new();
        app.agent_navigation.upsert(
            thread_id,
            Some("hidden".to_string()),
            /*agent_role*/ None,
            is_closed,
        );
        app.agent_navigation
            .set_picker_thread_visibility(thread_id, AgentPickerThreadVisibility::Hidden);
    }
    let live = app.agent_picker_selection_view_params(/*selected*/ None);
    let history = app.agent_history_selection_view_params(/*selected*/ None);
    assert_eq!((live.items.len(), history.items.len()), (0, 0));
}

#[tokio::test]
async fn agent_picker_more_opens_history_without_dismissing_parent() {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    app.agent_navigation.clear();
    let loaded = ThreadId::new();
    app.agent_navigation.upsert(
        loaded,
        Some("loaded".to_string()),
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    let history = ThreadId::new();
    app.agent_navigation.upsert(
        history,
        Some("history".to_string()),
        /*agent_role*/ None,
        /*is_closed*/ true,
    );
    let params = app.agent_picker_selection_view_params(/*selected*/ Some(1));
    app.chat_widget.show_selection_view(params);
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(app_event_rx.try_recv(), Ok(AppEvent::OpenAgentHistory));
    assert_eq!(
        app.chat_widget
            .selected_index_for_present_view("agent-picker"),
        Some(1)
    );
}

#[tokio::test]
async fn agent_picker_status_updates_without_switching_threads() -> Result<()> {
    let mut app = make_test_app().await;
    let app_server =
        crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let primary = ThreadId::new();
    app.enqueue_primary_thread_session(
        test_thread_session(primary, test_path_buf("/tmp/project")),
        Vec::new(),
    )
    .await?;
    let child = ThreadId::new();
    app.agent_navigation.upsert(
        child,
        Some("worker".into()),
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    app.thread_event_channels.insert(
        child,
        ThreadEventChannel::new_with_session(
            /*capacity*/ 8,
            test_thread_session(child, test_path_buf("/tmp/project")),
            vec![test_turn("turn-1", TurnStatus::Completed, Vec::new())],
        ),
    );
    let params = app.agent_picker_selection_view_params(Some(0));
    app.chat_widget.show_selection_view(params);
    assert!(render_bottom_popup(&app.chat_widget, /*width*/ 100).contains("[done]        worker"));

    for (status, expected) in [
        (
            codex_app_server_protocol::ThreadStatus::Idle,
            "[done]        worker",
        ),
        (
            codex_app_server_protocol::ThreadStatus::Active {
                active_flags: Vec::new(),
            },
            "[running]     worker",
        ),
        (
            codex_app_server_protocol::ThreadStatus::Idle,
            "[idle]        worker",
        ),
        (
            codex_app_server_protocol::ThreadStatus::SystemError,
            "[error]       worker",
        ),
    ] {
        app.handle_app_server_event(
            &app_server,
            codex_app_server_client::AppServerEvent::ServerNotification(Box::new(
                ServerNotification::ThreadStatusChanged(
                    codex_app_server_protocol::ThreadStatusChangedNotification {
                        thread_id: child.to_string(),
                        status,
                    },
                ),
            )),
        )
        .await;
        assert!(render_bottom_popup(&app.chat_widget, /*width*/ 100).contains(expected));
        assert_eq!(
            app.chat_widget
                .selected_index_for_present_view("agent-picker"),
            Some(0)
        );
    }
    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerNotification(Box::new(
            ServerNotification::ThreadStatusChanged(
                codex_app_server_protocol::ThreadStatusChangedNotification {
                    thread_id: child.to_string(),
                    status: codex_app_server_protocol::ThreadStatus::NotLoaded,
                },
            ),
        )),
    )
    .await;
    let rendered = render_bottom_popup(&app.chat_widget, /*width*/ 100);
    assert!(rendered.contains("more..."));
    assert!(!rendered.contains("[not loaded]  worker"));
    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerNotification(Box::new(
            turn_started_notification(child, "turn-2"),
        )),
    )
    .await;
    app.handle_app_server_event(
        &app_server,
        codex_app_server_client::AppServerEvent::ServerNotification(Box::new(
            turn_completed_notification(child, "turn-2", TurnStatus::Interrupted),
        )),
    )
    .await;
    assert!(render_bottom_popup(&app.chat_widget, /*width*/ 100).contains("[interrupted] worker"));
    app_server.shutdown().await?;
    Ok(())
}

#[test]
fn agent_picker_outcome_survives_buffer_eviction_and_ignores_old_completion() {
    let child = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ 1);
    store.push_notification(turn_started_notification(child, "current"));
    store.push_notification(turn_completed_notification(
        child,
        "older",
        TurnStatus::Completed,
    ));
    assert_eq!(store.latest_turn_status, Some(TurnStatus::InProgress));
    store.push_notification(turn_completed_notification(
        child,
        "current",
        TurnStatus::Interrupted,
    ));
    store.buffer.clear();
    assert_eq!(store.latest_turn_status, Some(TurnStatus::Interrupted));
    store.set_turns(vec![test_turn("snapshot", TurnStatus::Failed, Vec::new())]);
    assert_eq!(store.latest_turn_status, Some(TurnStatus::Failed));
}

#[tokio::test]
async fn agent_picker_busy_store_does_not_block_or_infer_completion() {
    let mut app = make_test_app().await;
    let child = ThreadId::new();
    app.agent_navigation.upsert(
        child, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.thread_event_channels
        .insert(child, ThreadEventChannel::new(/*capacity*/ 4));
    let store = Arc::clone(&app.thread_event_channels[&child].store);
    let _guard = store.lock().await;
    let entry = app.agent_navigation.get(&child).expect("picker entry");
    assert_eq!(
        app.agent_picker_status(child, entry),
        AgentPickerStatus::Unknown
    );
    app.agent_navigation.mark_running(child);
    let entry = app.agent_navigation.get(&child).expect("picker entry");
    assert_eq!(
        app.agent_picker_status(child, entry),
        AgentPickerStatus::Running
    );
}
