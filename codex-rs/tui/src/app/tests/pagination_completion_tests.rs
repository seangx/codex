//! Completed older pages keep each footer after its answer, including overlapping pages.

use super::session_lifecycle_requests::start_recording_app_server;
use super::*;
use app_test_support::create_fake_paginated_rollout;
use app_test_support::rollout_path;
use chrono::TimeZone;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput as CoreUserInput;
use codex_state::SqliteConfig;

#[tokio::test]
async fn older_pagination_completion_footers_follow_answers_without_overlap_duplicates()
-> Result<()> {
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    app.local_settings.tui.terminal_resize_reflow_max_rows = Some(2);
    let completed_at = chrono::Local
        .with_ymd_and_hms(
            /*year*/ 2000, /*month*/ 9, /*day*/ 6, /*hour*/ 14,
            /*min*/ 32, /*sec*/ 0,
        )
        .single()
        .expect("unambiguous local completion time");
    let timestamp = completed_at.to_rfc3339();
    let filename_timestamp = "2000-09-06T14-32-00";
    let thread_id = create_fake_paginated_rollout(
        codex_home.path(),
        filename_timestamp,
        &timestamp,
        "completed pagination",
        Some(app.config.model_provider_id.as_str()),
        /*git_info*/ None,
    )
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    let path = rollout_path(codex_home.path(), filename_timestamp, &thread_id);
    let thread_id = ThreadId::from_string(&thread_id)?;
    let mut records = std::fs::read_to_string(&path)?
        .lines()
        .take(/*n*/ 1)
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    for (index, name) in ["Oldest", "Middle", "Newest"].into_iter().enumerate() {
        let turn_id = format!("turn-{index}");
        let finished = completed_at.timestamp() + index as i64 * 60;
        let mut events = vec![EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: turn_id.clone(),
            root_turn_id: None,
            trace_id: None,
            started_at: Some(finished - 125),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })];
        let items = [
            TurnItem::UserMessage(UserMessageItem {
                id: format!("prompt-{index}"),
                client_id: None,
                content: vec![CoreUserInput::Text {
                    text: format!("{name} prompt"),
                    text_elements: Vec::new(),
                }],
            }),
            TurnItem::CommandExecution(CommandExecutionItem {
                id: format!("command-{index}"),
                plugin_id: None,
                script_path: None,
                model_context: None,
                process_id: None,
                command: vec!["printf".to_string(), format!("{name}-command-output")],
                cwd: codex_utils_path_uri::PathUri::from_abs_path(&app.config.cwd),
                parsed_cmd: vec![codex_protocol::parse_command::ParsedCommand::Unknown {
                    cmd: format!("printf {name}-command-output"),
                }],
                source: codex_protocol::protocol::ExecCommandSource::Agent,
                interaction_input: None,
                status: CommandExecutionStatus::Completed,
                stdout: None,
                stderr: None,
                aggregated_output: Some(format!("{name}-command-output")),
                exit_code: Some(0),
                duration: Some(std::time::Duration::from_millis(100)),
                formatted_output: None,
            }),
            TurnItem::AgentMessage(AgentMessageItem {
                id: format!("answer-{index}"),
                content: vec![AgentMessageContent::Text {
                    text: format!("{name} answer"),
                }],
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            }),
        ];
        // Keep the newest turns' original item counts so the first older page still contains
        // the middle answer, which the next page deliberately repeats.
        events.extend(
            items
                .into_iter()
                .filter(|item| index == 0 || !matches!(item, TurnItem::CommandExecution(_)))
                .map(|item| {
                    EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id,
                        turn_id: turn_id.clone(),
                        item,
                        started_at_ms: None,
                        completed_at_ms: finished * 1_000,
                    })
                }),
        );
        events.push(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id,
            last_agent_message: Some(format!("{name} answer")),
            error: None,
            started_at: Some(finished - 125),
            completed_at: Some(finished),
            duration_ms: Some(125_000),
            time_to_first_token_ms: None,
        }));
        for event in events {
            records.push(serde_json::json!({
                "timestamp": timestamp,
                "ordinal": records.len(),
                "type": "event_msg",
                "payload": event,
            }));
        }
    }
    let records = records
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{records}\n"))?;
    let (mut app_server, _requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let started = app_server
        .resume_thread(
            &app.local_settings,
            app.config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    // Snapshot only the prepended pages; the newest turn is already hydrated in the store.
    app.local_settings.tui.terminal_resize_reflow_max_rows = None;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.open_transcript_overlay(&mut tui);
    let mut overlapping_answer = None;
    let mut snapshots = Vec::new();
    for limit in [1, 100] {
        let cursor = app_server
            .begin_older_history_page(thread_id)
            .expect("older page");
        let mut page: ThreadItemsListResponse = app_server
            .thread_items_page(
                thread_id,
                /*turn_id*/ None,
                Some(cursor.clone()),
                limit,
            )
            .await?;
        if let Some(answer) = overlapping_answer.take() {
            // The later response repeats the preceding page's completed answer.
            page.data.insert(/*index*/ 0, answer);
        } else {
            overlapping_answer = page.data.first().cloned();
        }
        app.handle_older_history_page(&mut tui, &mut app_server, thread_id, &cursor, Ok(page))
            .await?;
        snapshots.push(
            app.render_transcript_lines_for_reflow(/*width*/ 80)
                .lines
                .iter()
                .map(rendered_line_text)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let compact = snapshots.join("\n\n--- next overlapping page ---\n\n");
    assert!(compact.contains("Ran 1 shell command"));
    assert!(!compact.contains("command-output"));
    insta::assert_snapshot!(compact);
    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}
