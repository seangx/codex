use super::*;
use codex_app_server_protocol::CommandAction;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;
use ratatui::text::Line;

fn command(id: &str, source: CommandExecutionSource, status: CommandExecutionStatus) -> ThreadItem {
    let exit_code = match status {
        CommandExecutionStatus::Completed => Some(0),
        CommandExecutionStatus::Failed => Some(1),
        CommandExecutionStatus::Declined | CommandExecutionStatus::InProgress => None,
    };
    ThreadItem::CommandExecution {
        id: id.to_string(),
        plugin_id: None,
        script_path: None,
        model_context: None,
        sandbox_type: None,
        command: format!("printf {id}"),
        cwd: LegacyAppPathString::from_string("/workspace"),
        process_id: None,
        source,
        status,
        command_actions: vec![CommandAction::Unknown {
            command: format!("printf {id}"),
        }],
        aggregated_output: Some(format!("output-{id}")),
        exit_code,
        duration_ms: Some(100),
    }
}

fn project(items: Vec<ThreadItem>) -> TranscriptCells {
    thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &AbsolutePathBuf::current_dir().expect("current directory"),
        items,
        RawReasoningVisibility::Hidden,
        /*config*/ None,
    )
}

fn text(lines: Vec<Line<'static>>) -> String {
    lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn completed_commands_are_compact_but_keep_full_output() {
    let cells = project(vec![
        command(
            "first",
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Completed,
        ),
        command(
            "second",
            CommandExecutionSource::UnifiedExecStartup,
            CommandExecutionStatus::Completed,
        ),
    ]);
    assert_eq!(cells.len(), 1);
    let compact = text(cells[0].display_lines(/*width*/ 80));
    let full = text(cells[0].transcript_lines(/*width*/ 80));
    assert!(!compact.contains("output-first"));
    assert!(!compact.contains("output-second"));
    assert!(full.contains("printf first"));
    assert!(full.contains("output-first"));
    assert!(full.contains("printf second"));
    assert!(full.contains("output-second"));
    insta::assert_snapshot!(format!("COMPACT\n{compact}\n\nFULL\n{full}"));
}

#[test]
fn failed_commands_break_groups_and_keep_error_visible() {
    let cells = project(vec![
        command(
            "before",
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Completed,
        ),
        command(
            "failure",
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Failed,
        ),
        command(
            "after",
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Completed,
        ),
    ]);
    assert_eq!(cells.len(), 3);
    let compact = cells
        .iter()
        .map(|cell| text(cell.display_lines(/*width*/ 80)))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(compact.contains("output-failure"));
    assert!(!compact.contains("output-before"));
    assert!(!compact.contains("output-after"));
    insta::assert_snapshot!(compact);
}

#[test]
fn user_shell_commands_stay_separate_from_agent_groups() {
    let cells = project(vec![
        command(
            "agent",
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Completed,
        ),
        command(
            "manual",
            CommandExecutionSource::UserShell,
            CommandExecutionStatus::Completed,
        ),
    ]);
    assert_eq!(cells.len(), 2);
    let compact = text(cells[1].display_lines(/*width*/ 80));
    assert!(compact.contains("You ran"));
    assert!(compact.contains("output-manual"));
    insta::assert_snapshot!(compact);
}

#[test]
fn separate_pages_do_not_share_pending_command_groups() {
    let first = project(vec![command(
        "first",
        CommandExecutionSource::Agent,
        CommandExecutionStatus::Completed,
    )]);
    let second = project(vec![command(
        "second",
        CommandExecutionSource::Agent,
        CommandExecutionStatus::Completed,
    )]);
    assert_eq!((first.len(), second.len()), (1, 1));
    assert!(!text(first[0].transcript_lines(/*width*/ 80)).contains("output-second"));
    assert!(!text(second[0].transcript_lines(/*width*/ 80)).contains("output-first"));
}

#[test]
fn persisted_read_and_search_actions_keep_their_summary() {
    let mut read = command(
        "read",
        CommandExecutionSource::Agent,
        CommandExecutionStatus::Completed,
    );
    let mut search = command(
        "search",
        CommandExecutionSource::Agent,
        CommandExecutionStatus::Completed,
    );
    if let ThreadItem::CommandExecution {
        command_actions, ..
    } = &mut read
    {
        *command_actions = vec![CommandAction::Read {
            command: "cat README.md".to_string(),
            name: "README.md".to_string(),
            path: LegacyAppPathString::from_string("/workspace/README.md"),
        }];
    }
    if let ThreadItem::CommandExecution {
        command_actions, ..
    } = &mut search
    {
        *command_actions = vec![CommandAction::Search {
            command: "rg needle".to_string(),
            query: Some("needle".to_string()),
            path: None,
        }];
    }
    let cells = project(vec![read, search]);
    assert_eq!(cells.len(), 1);
    insta::assert_snapshot!(text(cells[0].display_lines(/*width*/ 80)));
    let full = text(cells[0].transcript_lines(/*width*/ 80));
    assert!(full.contains("output-read"));
    assert!(full.contains("output-search"));
}

#[test]
fn nonzero_exit_is_not_hidden_by_completed_status() {
    let mut item = command(
        "failure",
        CommandExecutionSource::Agent,
        CommandExecutionStatus::Completed,
    );
    if let ThreadItem::CommandExecution { exit_code, .. } = &mut item {
        *exit_code = Some(2);
    }
    let cells = project(vec![item]);
    let compact = text(cells[0].display_lines(/*width*/ 80));
    assert!(compact.contains("output-failure"));
    insta::assert_snapshot!(compact);
}

#[test]
fn command_only_turns_remain_separate_in_full_thread_projection() {
    let turns = ["first", "second"].map(|id| serde_json::json!({
        "id": id,
        "items": [command(id, CommandExecutionSource::Agent, CommandExecutionStatus::Completed)],
        "status": "completed",
    }));
    let thread: Thread = serde_json::from_value(serde_json::json!({
        "id": ThreadId::new().to_string(),
        "sessionId": "session",
        "preview": "",
        "ephemeral": false,
        "modelProvider": "test",
        "createdAt": 0,
        "updatedAt": 0,
        "status": {"type": "idle"},
        "cwd": AbsolutePathBuf::current_dir().expect("current directory"),
        "cliVersion": "test",
        "source": "cli",
        "turns": turns,
    }))
    .expect("persisted thread fixture");
    let cells =
        thread_to_transcript_cells(thread, RawReasoningVisibility::Hidden, /*config*/ None);
    assert_eq!(cells.len(), 2);
    let full = cells
        .iter()
        .map(|cell| text(cell.transcript_lines(/*width*/ 80)))
        .collect::<Vec<_>>();
    assert!(full[0].contains("output-first"));
    assert!(!full[0].contains("output-second"));
    assert!(full[1].contains("output-second"));
    assert!(!full[1].contains("output-first"));
}
