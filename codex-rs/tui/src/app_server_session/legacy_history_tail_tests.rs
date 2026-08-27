use super::*;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Write;

fn message_line(kind: &str, message: &str) -> String {
    json!({
        "timestamp": "2026-08-27T00:00:00Z",
        "type": "event_msg",
        "payload": {
            "type": kind,
            "message": message,
            "kind": "plain"
        }
    })
    .to_string()
}

#[test]
fn reads_recent_messages_without_loading_the_rollout_prefix() -> io::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let path = temp_dir.path().join("large-legacy-rollout.jsonl");
    let mut file = File::create(&path)?;
    file.write_all(&vec![b'x'; LEGACY_HISTORY_TAIL_BYTES + 1024])?;
    writeln!(file)?;
    writeln!(file, "{}", message_line("user_message", "recent user"))?;
    writeln!(file, "{}", message_line("agent_message", "recent agent"))?;
    file.flush()?;

    let turns = read_recent_legacy_turns(path)?;
    let text = turns
        .into_iter()
        .flat_map(|turn| turn.items)
        .filter_map(|item| match item {
            ThreadItem::UserMessage { content, .. } => content.into_iter().find_map(|input| {
                let UserInput::Text { text, .. } = input else {
                    return None;
                };
                Some(text)
            }),
            ThreadItem::AgentMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(text, vec!["recent user", "recent agent"]);
    Ok(())
}

#[test]
fn ignores_a_partial_trailing_record() -> io::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let path = temp_dir.path().join("active-legacy-rollout.jsonl");
    let mut file = File::create(&path)?;
    writeln!(file, "{}", message_line("user_message", "complete"))?;
    write!(file, "{{\"timestamp\":")?;
    file.flush()?;

    let turns = read_recent_legacy_turns(path)?;

    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].items.len(), 1);
    Ok(())
}
