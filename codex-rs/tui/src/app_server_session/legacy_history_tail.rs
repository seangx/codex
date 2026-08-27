use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::Turn;
use codex_app_server_protocol::build_turns_from_rollout_items;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;

const LEGACY_HISTORY_TAIL_BYTES: usize = 16 * 1024 * 1024;
const LEGACY_HISTORY_MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
const LEGACY_HISTORY_MAX_MESSAGES: usize = 200;
const LEGACY_HISTORY_MAX_USER_MESSAGES: usize = 5;

pub(super) async fn load_recent_legacy_turns(path: &Path) -> io::Result<Vec<Turn>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_recent_legacy_turns(path))
        .await
        .map_err(io::Error::other)?
}

fn read_recent_legacy_turns(path: PathBuf) -> io::Result<Vec<Turn>> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let read_len = usize::try_from(file_len.min(LEGACY_HISTORY_TAIL_BYTES as u64))
        .map_err(io::Error::other)?;
    let start = file_len.saturating_sub(read_len as u64);
    file.seek(SeekFrom::Start(start))?;

    let mut bytes = vec![0; read_len];
    file.read_exact(&mut bytes)?;
    let bytes = if start != 0 {
        let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        &bytes[first_newline.saturating_add(1)..]
    } else {
        bytes.as_slice()
    };

    let mut messages = VecDeque::new();
    let mut user_messages = 0usize;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() || line.len() > LEGACY_HISTORY_MAX_RECORD_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_slice(line) else {
            // The app-server may append while the tail snapshot is being read. Ignore a partial
            // final record instead of making resume fail.
            continue;
        };
        let Ok(line) = codex_rollout::decode_rollout_line(value) else {
            continue;
        };
        let is_user_message = matches!(&line.item, RolloutItem::EventMsg(EventMsg::UserMessage(_)));
        let is_conversation_message = is_user_message
            || matches!(&line.item, RolloutItem::EventMsg(EventMsg::AgentMessage(_)));
        if !is_conversation_message {
            continue;
        }

        if is_user_message {
            user_messages = user_messages.saturating_add(1);
        }
        messages.push_back(line.item);
        while messages.len() > LEGACY_HISTORY_MAX_MESSAGES
            || user_messages > LEGACY_HISTORY_MAX_USER_MESSAGES
        {
            if matches!(
                messages.pop_front(),
                Some(RolloutItem::EventMsg(EventMsg::UserMessage(_)))
            ) {
                user_messages = user_messages.saturating_sub(1);
            }
        }
    }

    Ok(build_turns_from_rollout_items(
        &messages.into_iter().collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
#[path = "legacy_history_tail_tests.rs"]
mod tests;
