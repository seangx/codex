use super::super::ResponsesWebsocketTimingLogContext;
use super::super::WsCommand;
use super::super::WsStream;
use super::super::connection_diagnostics::ConnectionDiagnostics;
use super::super::run_websocket_response_stream;
use crate::error::ApiError;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_websocket_client::WebSocketConnector;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::Subscriber;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::span;

type LogFields = BTreeMap<String, String>;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<LogFields>>>);

impl Capture {
    fn events(&self, message: &str) -> Vec<LogFields> {
        self.0
            .lock()
            .expect("capture lock should not be poisoned")
            .iter()
            .filter(|fields| fields.get("message").map(String::as_str) == Some(message))
            .cloned()
            .collect()
    }
}

struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

enum Exchange {
    SendError,
    SendTimeout,
    ReceiveTimeout,
    Messages(Vec<Message>),
}

async fn exchange(
    exchange: Exchange,
    previous_response_id: Option<String>,
) -> (Result<(), ApiError>, Vec<LogFields>) {
    let capture = Capture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (tx_command, mut rx_command) = mpsc::channel(1);
    let (tx_message, rx_message) = mpsc::unbounded_channel();
    let pump_task = tokio::spawn(async move {
        let WsCommand::Send { tx_result, .. } = rx_command.recv().await.unwrap();
        match exchange {
            Exchange::SendError => {
                let _ = tx_result.send(Err(WsError::Io(std::io::Error::from(
                    std::io::ErrorKind::BrokenPipe,
                ))));
            }
            Exchange::SendTimeout => {
                std::future::pending::<()>().await;
                drop(tx_result);
            }
            Exchange::ReceiveTimeout => {
                let _ = tx_result.send(Ok(()));
                std::future::pending::<()>().await;
                drop(tx_message);
            }
            Exchange::Messages(messages) => {
                let _ = tx_result.send(Ok(()));
                for message in messages {
                    tx_message.send(Ok(message)).unwrap();
                }
            }
        }
    });
    let mut stream = WsStream {
        connection_id: 17,
        tx_command,
        rx_message,
        pump_task,
    };
    let context = ResponsesWebsocketTimingLogContext {
        model: "private-model".into(),
        session_id: None,
        thread_id: None,
        turn_id: None,
        traceparent: None,
        previous_response_id,
        request_start_ms: None,
        warmup: false,
        connection_reused: false,
        input_items: 1,
    };
    let (tx_event, _rx_event) = mpsc::channel(16);
    let result = run_websocket_response_stream(
        &mut stream,
        tx_event,
        "private-image-图片".into(),
        Duration::from_millis(250),
        /*telemetry*/ None,
        /*turn_state*/ None,
        &context,
    )
    .await;
    let logs = capture.0.lock().unwrap().clone();
    (result, logs)
}

#[tokio::test]
async fn reports_close_after_created_without_logging_server_text_or_input() {
    let (result, logs) = exchange(
        Exchange::Messages(vec![
            Message::Text(
                r#"{"type":"response.created","response":{"id":"private-response"}}"#.into(),
            ),
            Message::Close(Some(CloseFrame {
                code: CloseCode::Size,
                reason: "message too large; private-server-text".into(),
            })),
        ]),
        Some("private-previous-response".into()),
    )
    .await;
    assert!(result.is_err());
    let summary = logs.last().unwrap();
    let selected: BTreeMap<_, _> = [
        "request_bytes",
        "input_items",
        "has_previous_response_id",
        "connection_reused",
        "outcome",
        "response_created",
        "close_code",
        "close_reason_category",
    ]
    .into_iter()
    .map(|key| (key, summary.get(key).unwrap().as_str()))
    .collect();
    let bytes = "private-image-图片".len().to_string();
    assert_eq!(
        selected,
        BTreeMap::from([
            ("request_bytes", bytes.as_str()),
            ("input_items", "1"),
            ("has_previous_response_id", "true"),
            ("connection_reused", "false"),
            ("outcome", "\"server_close\""),
            ("response_created", "true"),
            ("close_code", "1009"),
            ("close_reason_category", "\"size_limit\""),
        ])
    );
    assert!(summary.contains_key("send_ms"));
    assert!(summary.contains_key("first_message_ms"));
    assert!(!format!("{logs:?}").contains("private-"));
    assert_eq!(
        logs.first().unwrap().get("ws_request_id"),
        summary.get("ws_request_id")
    );
    for log in &logs {
        assert_eq!(log.get("ws_connection_id").map(String::as_str), Some("17"));
    }
}

#[tokio::test]
async fn malformed_event_and_unknown_close_reason_are_redacted() {
    let (result, logs) = exchange(
        Exchange::Messages(vec![
            Message::Text(
                r#"{"type":"response.created","content_index":"private-invalid-number"}"#.into(),
            ),
            Message::Close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "private-close-text".into(),
            })),
        ]),
        /*previous_response_id*/ None,
    )
    .await;
    assert!(result.is_err());
    assert!(!format!("{logs:?}").contains("private-"));
    assert_eq!(
        logs.last()
            .unwrap()
            .get("close_reason_category")
            .map(String::as_str),
        Some("\"other_redacted\"")
    );
}

#[tokio::test]
async fn distinguishes_send_failure_send_timeout_and_receive_timeout() {
    for (exchange_kind, expected) in [
        (Exchange::SendError, "\"send_error\""),
        (Exchange::SendTimeout, "\"send_timeout\""),
        (Exchange::ReceiveTimeout, "\"receive_timeout\""),
    ] {
        let (result, logs) = exchange(exchange_kind, /*previous_response_id*/ None).await;
        assert!(result.is_err());
        let summary = logs.last().unwrap();
        assert_eq!(summary.get("outcome").map(String::as_str), Some(expected));
        assert_eq!(
            summary.get("has_previous_response_id").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            summary.get("response_created").map(String::as_str),
            Some("false")
        );
        assert!(!summary.contains_key("first_message_ms"));
    }
}

#[tokio::test]
async fn distinguishes_completed_response_from_empty_close_and_eof() {
    for (messages, expected) in [
        (
            vec![Message::Text(
                r#"{"type":"response.completed","response":{"id":"resp-test"}}"#.into(),
            )],
            "\"completed\"",
        ),
        (vec![Message::Close(None)], "\"server_close\""),
        (vec![], "\"stream_eof\""),
    ] {
        let (result, logs) = exchange(
            Exchange::Messages(messages),
            /*previous_response_id*/ None,
        )
        .await;
        assert_eq!(result.is_ok(), expected == "\"completed\"");
        assert_eq!(
            logs.last().unwrap().get("outcome").map(String::as_str),
            Some(expected)
        );
    }
}

async fn websocket_pair() -> (
    WsStream,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener should bind");
    let request = format!(
        "ws://{}/responses",
        listener
            .local_addr()
            .expect("listener should have an address")
    )
    .into_client_request()
    .expect("websocket request should build");
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let connector = WebSocketConnector::new(&factory).expect("connector should build");
    let accept = async {
        let (socket, _) = listener
            .accept()
            .await
            .expect("loopback connection should be accepted");
        tokio_tungstenite::accept_async_with_config(socket, Some(WebSocketConfig::default()))
            .await
            .expect("server websocket handshake should succeed")
    };
    let (client, server) = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        tokio::join!(
            connector.connect_loopback_direct(request, WebSocketConfig::default()),
            accept
        )
    })
    .await
    .expect("websocket pair should connect before timeout");
    (
        WsStream::new(
            client.expect("client websocket handshake should succeed").0,
            Duration::from_secs(/*secs*/ 60),
        ),
        server,
    )
}

#[tokio::test]
async fn idle_server_close_is_logged_after_completed_request_without_another_request() {
    for (reason, category) in [
        ("private-idle-close-text", "\"other_redacted\""),
        (
            "connection limit reached (60 minutes); private-idle-close-text",
            "\"connection_lifetime_limit\"",
        ),
    ] {
        let capture = Capture::default();
        let _subscriber = tracing::subscriber::set_default(capture.clone());
        let (mut stream, mut server) = websocket_pair().await;
        let connection_id = stream.connection_id.to_string();
        let (tx_close, rx_close) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            assert!(matches!(
                server
                    .next()
                    .await
                    .expect("request frame should arrive")
                    .expect("request frame should be valid"),
                Message::Text(_)
            ));
            server
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"resp-test"}}"#.into(),
                ))
                .await
                .expect("completed response should send");
            rx_close.await.expect("test should release the idle close");
            server
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: reason.into(),
                })))
                .await
                .expect("idle close should send");
        });
        let context = ResponsesWebsocketTimingLogContext {
            model: "private-model".into(),
            session_id: None,
            thread_id: None,
            turn_id: None,
            traceparent: None,
            previous_response_id: None,
            request_start_ms: None,
            warmup: false,
            connection_reused: false,
            input_items: 1,
        };
        let (tx_event, _rx_event) = mpsc::channel(/*buffer*/ 16);
        run_websocket_response_stream(
            &mut stream,
            tx_event,
            "private-request".into(),
            Duration::from_secs(/*secs*/ 5),
            /*telemetry*/ None,
            /*turn_state*/ None,
            &context,
        )
        .await
        .expect("response should complete before the idle close");
        assert!(capture.events("websocket connection finished").is_empty());
        let requests = capture.events("websocket request finished");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].get("outcome").map(String::as_str),
            Some("\"completed\"")
        );
        assert_eq!(requests[0].get("ws_connection_id"), Some(&connection_id));
        tx_close
            .send(())
            .expect("server should still be waiting for the close signal");
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), &mut stream.pump_task)
            .await
            .expect("pump should finish after the idle close")
            .expect("pump should not panic");
        server_task.await.expect("server task should not panic");
        drop(stream);
        let finished = capture.events("websocket connection finished");
        assert_eq!(finished.len(), 1);
        let selected: BTreeMap<_, _> = [
            "ws_connection_id",
            "outcome",
            "close_code",
            "close_reason_category",
            "close_reason_bytes",
        ]
        .into_iter()
        .map(|key| {
            (
                key,
                finished[0]
                    .get(key)
                    .expect("connection finish should contain the close field")
                    .as_str(),
            )
        })
        .collect();
        let reason_bytes = reason.len().to_string();
        assert_eq!(
            selected,
            BTreeMap::from([
                ("ws_connection_id", connection_id.as_str()),
                ("outcome", "\"server_close\""),
                ("close_code", "1008"),
                ("close_reason_category", category),
                ("close_reason_bytes", reason_bytes.as_str()),
            ])
        );
        assert_eq!(capture.events("websocket request started").len(), 1);
        let established = capture.events("websocket connection established");
        assert_eq!(established.len(), 1);
        assert_eq!(established[0].get("ws_connection_id"), Some(&connection_id));
        assert!(!format!("{established:?}{finished:?}{requests:?}").contains("private-"));
    }
}

#[tokio::test]
async fn local_stream_drop_logs_connection_finish_exactly_once() {
    let capture = Capture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (stream, _server) = websocket_pair().await;
    let connection_id = stream.connection_id.to_string();
    let pump = stream.pump_task.abort_handle();
    drop(stream);
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        while !pump.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted pump should finish before timeout");
    let finished = capture.events("websocket connection finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].get("ws_connection_id"), Some(&connection_id));
    assert_eq!(
        finished[0].get("outcome").map(String::as_str),
        Some("\"local_drop\"")
    );
}

#[tokio::test]
async fn connection_guard_reports_eof_and_redacted_keepalive_errors_with_counters() {
    for outcome in ["stream_eof", "ping_send_error"] {
        let capture = Capture::default();
        let _subscriber = tracing::subscriber::set_default(capture.clone());
        let mut diagnostics =
            ConnectionDiagnostics::new(Duration::from_millis(/*millis*/ 250));
        diagnostics.received();
        diagnostics.ping_sent();
        diagnostics.pong_received();
        if outcome == "ping_send_error" {
            diagnostics.observe_error(
                outcome,
                &WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "private-network-error",
                )),
            );
        } else {
            diagnostics.finish(outcome);
        }
        drop(diagnostics);
        let finished = capture.events("websocket connection finished");
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].get("outcome"), Some(&format!("{outcome:?}")));
        let selected: BTreeMap<_, _> = ["keepalive_interval_ms", "pings_sent", "pongs_received"]
            .into_iter()
            .map(|key| {
                (
                    key,
                    finished[0]
                        .get(key)
                        .expect("connection finish should contain the counter")
                        .as_str(),
                )
            })
            .collect();
        assert_eq!(
            selected,
            BTreeMap::from([
                ("keepalive_interval_ms", "250"),
                ("pings_sent", "1"),
                ("pongs_received", "1"),
            ])
        );
        assert!(finished[0].contains_key("lifetime_ms"));
        assert!(finished[0].contains_key("last_received_ms"));
        let (category, kind) = if outcome == "ping_send_error" {
            ("\"io\"", "Some(BrokenPipe)")
        } else {
            ("\"none\"", "None")
        };
        assert_eq!(
            finished[0].get("error_category").map(String::as_str),
            Some(category)
        );
        assert_eq!(
            finished[0].get("io_error_kind").map(String::as_str),
            Some(kind)
        );
        assert!(!format!("{finished:?}").contains("private-"));
    }
}
