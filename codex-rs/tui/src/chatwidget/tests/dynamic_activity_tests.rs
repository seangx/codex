//! Dynamic calls must finish visibly whether transcript rows are retained or terminal-owned.

use super::*;
use crate::local_settings::LocalSettings;
use crate::transcript_mode::TranscriptMode;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_config::types::AltScreenMode;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

fn dynamic_item(id: &str, status: DynamicToolCallStatus) -> AppServerThreadItem {
    let completed = status != DynamicToolCallStatus::InProgress;
    AppServerThreadItem::DynamicToolCall {
        id: id.to_string(),
        namespace: Some("test".to_string()),
        tool: "lookup".to_string(),
        arguments: json!({ "id": id }),
        status,
        content_items: completed.then(|| {
            vec![DynamicToolCallOutputContentItem::InputText {
                text: format!("Result for {id}"),
            }]
        }),
        success: None,
        duration_ms: completed.then_some(/*t*/ 25),
    }
}

#[tokio::test]
async fn terminal_dynamic_activity_retains_calls_across_both_fallbacks() {
    let mut outputs = Vec::new();
    let mut details = Vec::new();
    for (owned_enabled, alternate_screen) in
        [(false, AltScreenMode::Always), (true, AltScreenMode::Never)]
    {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.config.tui_fullscreen_transcript = owned_enabled;
        chat.config.tui_alternate_screen = alternate_screen;
        chat.local_settings = LocalSettings::from(&chat.config);
        assert_eq!(
            chat.local_settings.transcript_mode,
            TranscriptMode::Terminal
        );
        drain_insert_history(&mut rx);

        for id in ["call-1", "call-2"] {
            chat.on_dynamic_tool_item(dynamic_item(id, DynamicToolCallStatus::InProgress));
        }
        assert!(drain_insert_history(&mut rx).is_empty());
        assert_eq!(chat.transcript.dynamic_calls.len(), 2);

        chat.on_dynamic_tool_item(dynamic_item("call-2", DynamicToolCallStatus::Completed));
        chat.on_dynamic_tool_item(dynamic_item("call-1", DynamicToolCallStatus::Failed));
        let cells = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(cell),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(cells.len(), 1);
        outputs.push(
            cells
                .iter()
                .flat_map(|cell| cell.display_lines(/*width*/ 80))
                .collect::<Vec<_>>(),
        );
        details.push(lines_to_single_string(
            &cells
                .iter()
                .flat_map(|cell| cell.transcript_lines(/*width*/ 80))
                .collect::<Vec<_>>(),
        ));
        assert!(chat.transcript.dynamic_calls.is_empty());
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(details[0], details[1]);
    assert!(details[0].contains("Failed test.lookup"), "{}", details[0]);
    assert!(details[0].contains("Result for call-1"), "{}", details[0]);
    assert!(details[0].contains("Called test.lookup"), "{}", details[0]);
    assert!(details[0].contains("Result for call-2"), "{}", details[0]);
    insta::assert_snapshot!(
        lines_to_single_string(&outputs[0]),
        @"
        • Called test.lookup 2 times
        "
    );
}

#[tokio::test]
async fn owned_dynamic_activity_updates_the_retained_row_after_config_refresh() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.transcript_mode = TranscriptMode::Owned;
    // A thread/config refresh cannot change ownership after the terminal starts.
    chat.config.tui_fullscreen_transcript = false;
    drain_insert_history(&mut rx);
    chat.on_dynamic_tool_item(dynamic_item("call-1", DynamicToolCallStatus::InProgress));
    chat.flush_active_cell();
    let retained = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(cell),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 1);
    assert_eq!(
        lines_to_single_string(&retained[0].display_lines(/*width*/ 80)),
        "• Called test.lookup\n",
    );

    chat.on_dynamic_tool_item(dynamic_item("call-1", DynamicToolCallStatus::Completed));
    assert!(drain_insert_history(&mut rx).is_empty());
    assert!(chat.transcript.dynamic_calls.is_empty());
    let transcript = lines_to_single_string(&retained[0].transcript_lines(/*width*/ 80));
    assert!(
        transcript.contains("Called test.lookup · 25ms"),
        "{transcript}"
    );
    assert!(transcript.contains("Result for call-1"), "{transcript}");
}

#[tokio::test]
async fn turn_finalization_drains_lifecycle_queue_before_interrupting_remaining_calls() {
    let (mut chat, mut events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    drain_insert_history(&mut events);
    chat.interrupts.push_user_input(ToolRequestUserInputParams {
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        item_id: "question-1".to_string(),
        questions: vec![ToolRequestUserInputQuestion {
            id: "scope".to_string(),
            header: "Scope".to_string(),
            question: "Which change should I review?".to_string(),
            is_other: false,
            is_secret: false,
            options: None,
        }],
        is_blocking: true,
        auto_resolution_ms: None,
    });
    chat.interrupts
        .push_item_started(dynamic_item("completed", DynamicToolCallStatus::InProgress));
    chat.interrupts
        .push_item_completed(dynamic_item("completed", DynamicToolCallStatus::Completed));
    chat.interrupts.push_item_started(dynamic_item(
        "unfinished",
        DynamicToolCallStatus::InProgress,
    ));
    chat.finalize_turn();
    assert!(chat.interrupts.has_pending_prompt());
    assert!(!chat.bottom_pane.has_active_view());
    let cells = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<history_cell::ToolActivityGroupCell>() =>
            {
                Some(cell)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), 1);
    assert!(chat.transcript.dynamic_calls.is_empty());
    let raw = lines_to_single_string(&cells[0].raw_lines());
    assert!(raw.contains("Result for completed"), "{raw}");
    assert!(
        raw.contains("Interrupted before this tool returned a result."),
        "{raw}"
    );
    let lines = cells[0].display_lines(/*width*/ 64);
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        /*width*/ 64,
        lines.len() as u16,
    );
    let mut buffer = Buffer::empty(area);
    Paragraph::new(lines).render(area, &mut buffer);
    insta::assert_snapshot!("interrupted_dynamic_activity", format!("{buffer:?}"));
}
