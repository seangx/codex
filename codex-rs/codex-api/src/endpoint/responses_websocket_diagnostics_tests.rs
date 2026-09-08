use super::super::ResponsesWebsocketTimingLogContext;
use super::super::WsCommand;
use super::super::WsStream;
use super::super::run_websocket_response_stream;
use crate::error::ApiError;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::Subscriber;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::span;

type LogFields = BTreeMap<String, String>;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<LogFields>>>);

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
