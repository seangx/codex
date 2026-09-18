use super::*;

#[tokio::test]
async fn image_result_live_and_replay_render_in_the_owning_call() {
    let item: AppServerThreadItem = serde_json::from_value(json!({
        "type": "mcpToolCall", "id": "image", "server": "node_repl", "tool": "js",
        "status": "completed", "arguments": {"title": "Inspect screenshot", "code": "showImage()"},
        "result": {"content": [
            {"type": "text", "text": "Screenshot captured"},
            {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="}
        ]},
        "durationMs": 5
    })).expect("MCP image result");
    let mut outputs = Vec::new();
    for replay in [false, true] {
        let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        if replay {
            chat.replay_thread_item(item.clone(), "turn-1".into(), ReplayKind::ThreadSnapshot);
        } else {
            let mut started = item.clone();
            if let AppServerThreadItem::McpToolCall { status, result, .. } = &mut started {
                *status = codex_app_server_protocol::McpToolCallStatus::InProgress;
                *result = None;
            }
            chat.on_mcp_tool_call_started(started);
            chat.on_mcp_tool_call_completed(item.clone());
        }
        chat.flush_active_cell();
        let mut cells = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                cells.push(cell);
            }
        }
        assert_eq!(cells.len(), 1);
        outputs.push(format!(
            "history:\n{}\ntranscript:\n{}",
            lines_to_single_string(&cells[0].display_lines(/*width*/ 100)),
            lines_to_single_string(&cells[0].transcript_lines(/*width*/ 100)),
        ));
    }
    assert_eq!(outputs[0], outputs[1]);
    insta::assert_snapshot!(outputs[0]);
}
