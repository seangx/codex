use super::*;
use codex_protocol::items::TurnItem;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;
use tokio::time::sleep;

const TOTAL_COMPACTION_DEADLINE_MS: u64 = 800;

fn gated_chunk(gate: oneshot::Receiver<()>, body: String) -> StreamingSseChunk {
    StreamingSseChunk {
        gate: Some(gate),
        body,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_total_deadline_survives_retry_and_progress_heartbeats() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let compacting = responses::sse(vec![json!({
        "type": "response.compaction.compacting"
    })]);
    let seed = responses::sse(vec![
        responses::ev_assistant_message("seed", "SEED_REPLY"),
        responses::ev_completed("seed-response"),
    ]);
    let follow_up = responses::sse(vec![
        responses::ev_assistant_message("follow-up", "FOLLOW_UP_REPLY"),
        responses::ev_completed("follow-up-response"),
    ]);
    let completed_compaction = responses::sse(vec![
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "compaction",
                "encrypted_content": "TOO_LATE_COMPACTION"
            }
        }),
        responses::ev_completed("late-compaction-response"),
    ]);

    let mut heartbeat_senders = Vec::new();
    let mut retry_chunks = Vec::new();
    for _ in 0..4 {
        let (tx, rx) = oneshot::channel();
        heartbeat_senders.push(tx);
        retry_chunks.push(gated_chunk(rx, compacting.clone()));
    }
    let (completion_tx, completion_rx) = oneshot::channel();
    retry_chunks.push(gated_chunk(completion_rx, completed_compaction));

    let (server, _completions) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: seed,
        }],
        // The first compaction stream closes after a non-terminal event, forcing a retry.
        vec![StreamingSseChunk {
            gate: None,
            body: compacting,
        }],
        retry_chunks,
        vec![StreamingSseChunk {
            gate: None,
            body: follow_up,
        }],
    ])
    .await;
    // The gated transport fixture is local-only; this case does not execute tools.
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.stream_idle_timeout_ms = Some(TOTAL_COMPACTION_DEADLINE_MS);
            config.model_provider.supports_websockets = false;
        })
        .build_with_streaming_server(&server)
        .await?;
    let codex = &test.codex;

    test.submit_turn("history before timed out compaction")
        .await?;
    codex.submit(Op::Compact).await?;
    wait_for_event(codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(started)
                if matches!(&started.item, TurnItem::ContextCompaction(_))
        )
    })
    .await;

    let heartbeat_task = tokio::spawn(async move {
        for (sender, delay_ms) in heartbeat_senders.into_iter().zip([300, 150, 150, 150]) {
            sleep(Duration::from_millis(delay_ms)).await;
            let _ = sender.send(());
        }
        sleep(Duration::from_millis(350)).await;
        let _ = completion_tx.send(());
    });

    let error_message = wait_for_event_match(codex, |event| match event {
        EventMsg::Error(error) if error.message.contains("total deadline") => {
            Some(error.message.clone())
        }
        _ => None,
    })
    .await;
    assert!(error_message.contains("waiting for response.completed"));
    wait_for_turn_complete(codex).await;
    assert_eq!(server.requests().await.len(), 3);

    test.submit_turn("follow up after timed out compaction")
        .await?;
    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);
    let follow_up_body = zstd::stream::decode_all(requests[3].as_slice())?;
    let follow_up_request: Value = serde_json::from_slice(&follow_up_body)?;
    let follow_up_body = follow_up_request.to_string();
    assert!(follow_up_body.contains("history before timed out compaction"));
    assert!(follow_up_body.contains("follow up after timed out compaction"));
    assert!(!follow_up_body.contains("TOO_LATE_COMPACTION"));

    heartbeat_task.abort();
    server.shutdown().await;
    Ok(())
}
