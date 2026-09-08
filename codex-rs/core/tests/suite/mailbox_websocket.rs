//! Queued child notifications preserve a completed WS response and its incremental continuation.

use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_response_created;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::extensions::ExtensionsConfig;
use tokio_tungstenite::tungstenite::extensions::compression::deflate::DeflateConfig;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

#[derive(Clone, Copy)]
enum ItemBoundary {
    Reasoning,
    Commentary,
}

async fn read_request(socket: &mut WebSocketStream<TcpStream>) -> anyhow::Result<Value> {
    loop {
        match timeout(Duration::from_secs(/*secs*/ 10), socket.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("WS closed before incremental continuation"))??
        {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Binary(bytes) => return Ok(serde_json::from_slice(&bytes)?),
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Pong(_) => {}
            other => anyhow::bail!("expected a WS request, got {other:?}"),
        }
    }
}

async fn send_events(
    socket: &mut WebSocketStream<TcpStream>,
    events: Vec<Value>,
) -> anyhow::Result<()> {
    for event in events {
        socket.send(Message::Text(event.to_string().into())).await?;
    }
    Ok(())
}

#[test_case(ItemBoundary::Reasoning; "reasoning")]
#[test_case(ItemBoundary::Commentary; "commentary")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_mail_preserves_websocket_incremental_continuation(
    boundary: ItemBoundary,
) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}/v1", listener.local_addr()?);
    let (mail_queued_tx, mail_queued_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    // The test deliberately accepts only one connection: the follow-up must use this socket.
    let mut server = tokio::spawn(async move {
        let mut socket = loop {
            let (stream, _) = listener.accept().await?;
            let mut extensions = ExtensionsConfig::default();
            extensions.permessage_deflate = Some(DeflateConfig::default());
            let mut config = WebSocketConfig::default();
            config.extensions = extensions;
            // As with the shared WS fixture, model probes must not consume the connection.
            if let Ok(socket) = accept_async_with_config(stream, Some(config)).await {
                break socket;
            }
        };
        let mut first = read_request(&mut socket).await?;
        if first["generate"] == false {
            send_events(
                &mut socket,
                vec![ev_response_created("warmup"), ev_completed("warmup")],
            )
            .await?;
            first = read_request(&mut socket).await?;
        }
        let (added, done) = match boundary {
            ItemBoundary::Reasoning => (
                ev_reasoning_item_added("item-1", &["thinking"]),
                ev_reasoning_item("item-1", &["thinking"], &[]),
            ),
            ItemBoundary::Commentary => {
                let mut added = ev_message_item_added("item-1", "working");
                let mut done = ev_assistant_message("item-1", "working");
                added["item"]["phase"] = json!("commentary");
                done["item"]["phase"] = json!("commentary");
                (added, done)
            }
        };
        send_events(&mut socket, vec![ev_response_created("resp-1"), added]).await?;
        mail_queued_rx.await?;
        // Mail is already pending when the item ends. The tool call after it must survive,
        // and response.completed must be consumed before sending the queued mail.
        let mut tool_call = ev_function_call(
            "plan-preserved",
            "update_plan",
            r#"{"plan":[{"step":"review child result","status":"in_progress"}]}"#,
        );
        // Real server output has an item ID. Omitting it would make local history assign a
        // synthetic ID and break prefix equality independently of mailbox delivery.
        tool_call["item"]["id"] = json!("fc-plan-preserved");
        send_events(&mut socket, vec![done, tool_call, ev_completed("resp-1")]).await?;
        let follow_up = read_request(&mut socket).await?;
        send_events(
            &mut socket,
            vec![
                ev_response_created("resp-2"),
                ev_assistant_message("final", "done"),
                ev_completed("resp-2"),
            ],
        )
        .await?;
        finished_rx.await?;
        Ok::<_, anyhow::Error>((first, follow_up))
    });

    let http = responses::start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(0);
            config.update_plan_enabled = true;
            config
                .features
                .enable(Feature::ResponsesWebsocketsV2)
                .expect("test config allows websocket feature");
        })
        .build_with_auto_env(&http)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "original user prompt".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::select! {
    _ = wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("model request failed before the item boundary: {error:?}");
        }
        matches!(event, EventMsg::ItemStarted(started) if matches!(
            (&boundary, &started.item),
            (ItemBoundary::Reasoning, TurnItem::Reasoning(_))
                | (ItemBoundary::Commentary, TurnItem::AgentMessage(_))
        ))
    }) => {},
    result = &mut server => anyhow::bail!("WS fixture ended before the item boundary: {result:?}"),
    }
    test.codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("valid worker path"),
                AgentPath::root(),
                Vec::new(),
                "queued child result".to_string(),
                /*trigger_turn*/ false,
            ),
            start_options: Default::default(),
        })
        .await?;
    test.codex
        .submit(Op::RealtimeConversationListVoices)
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::RealtimeConversationListVoicesResponse(_))
    })
    .await;
    mail_queued_tx.send(()).expect("release response item");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    finished_tx.send(()).expect("finish WS fixture");
    let (first, follow_up) = server.await??;

    assert_eq!(first["type"], "response.create");
    assert_eq!(follow_up["previous_response_id"], "resp-1");
    let delta = follow_up["input"].as_array().expect("incremental input");
    assert_eq!(
        delta
            .iter()
            .map(|item| item["type"].as_str())
            .collect::<Vec<_>>(),
        vec![Some("function_call_output"), Some("agent_message")]
    );
    assert_eq!(delta[0]["call_id"], "plan-preserved");
    assert_eq!(delta[0]["output"], "Plan updated");
    assert_eq!(delta[1]["author"], "/root/worker");
    assert_eq!(delta[1]["recipient"], "/root");
    assert_eq!(
        delta[1]["content"],
        json!([{"type": "input_text", "text": "queued child result"}])
    );
    Ok(())
}
