use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_model_provider_info::WireApi;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RequestUserInputEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestionOption;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_requests;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use tokio::time::Duration;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::http::Method;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const KEEP_RECONNECTING_LABEL: &str = "Keep reconnecting (Recommended)";
const USE_HTTPS_FALLBACK_LABEL: &str = "Use HTTPS fallback";

async fn start_turn(test: &TestCodex, prompt: &str) -> Result<()> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd.path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(test.cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    Ok(())
}

async fn wait_for_fallback_request(test: &TestCodex) -> RequestUserInputEvent {
    let EventMsg::RequestUserInput(request) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::RequestUserInput(_))
    })
    .await
    else {
        unreachable!("wait_for_event should return a request-user-input event")
    };
    assert_eq!(request.call_id, "websocket_fallback_choice");
    assert!(!request.turn_id.is_empty());
    assert!(request.is_blocking);
    assert_eq!(request.auto_resolution_ms, None);
    assert_eq!(
        request.questions,
        vec![RequestUserInputQuestion {
            id: "transport".to_string(),
            header: "Connection".to_string(),
            question: "WebSocket retries were exhausted. How should Codex continue?".to_string(),
            is_other: false,
            is_secret: false,
            options: Some(vec![
                RequestUserInputQuestionOption {
                    label: KEEP_RECONNECTING_LABEL.to_string(),
                    description: "Retry WebSocket after a longer delay and keep HTTPS disabled."
                        .to_string(),
                },
                RequestUserInputQuestionOption {
                    label: USE_HTTPS_FALLBACK_LABEL.to_string(),
                    description:
                        "Replay this turn over HTTPS now; long sessions may resend a large request."
                            .to_string(),
                },
            ]),
        }]
    );
    request
}

async fn answer_fallback_request(
    test: &TestCodex,
    request: RequestUserInputEvent,
    answer: &str,
) -> Result<()> {
    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "transport".to_string(),
                    RequestUserInputAnswer {
                        answers: vec![answer.to_string()],
                    },
                )]),
            },
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_on_upgrade_required_connect() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(ResponseTemplate::new(426))
        .mount(&server)
        .await;

    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            // If we don't treat 426 specially, the sampling loop would retry the WebSocket
            // handshake before switching to the HTTP transport.
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // The startup prewarm request sees 426 and immediately switches the session to HTTP fallback,
    // so the first turn goes straight to HTTP with no additional websocket connect attempt.
    assert_eq!(websocket_attempts, 1);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

async fn assert_session_asks_before_switching_from_websocket_to_http(
    session_source: SessionSource,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex()
        .with_session_source(session_source)
        .with_config({
            let base_url = format!("{}/v1", server.uri());
            move |config| {
                config.model_provider.base_url = Some(base_url);
                config.model_provider.wire_api = WireApi::Responses;
                config.model_provider.supports_websockets = true;
                config.model_provider.stream_max_retries = Some(0);
                config.model_provider.request_max_retries = Some(0);
            }
        });
    let test = builder.build(&server).await?;

    start_turn(&test, "hello").await?;
    let request = wait_for_fallback_request(&test).await;

    let requests_before_answer = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests_before_answer
            .iter()
            .filter(|request| {
                request.method == Method::POST && request.url.path().ends_with("/responses")
            })
            .count(),
        0,
        "HTTP fallback must wait for the user's choice"
    );

    answer_fallback_request(&test, request, USE_HTTPS_FALLBACK_LABEL).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|request| {
            request.method == Method::GET && request.url.path().ends_with("/responses")
        })
        .count();
    let http_attempts = requests
        .iter()
        .filter(|request| {
            request.method == Method::POST && request.url.path().ends_with("/responses")
        })
        .count();
    assert_eq!(websocket_attempts, 2);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_asks_before_switching_from_websocket_to_http() -> Result<()> {
    assert_session_asks_before_switching_from_websocket_to_http(SessionSource::Cli).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_server_asks_before_switching_from_websocket_to_http() -> Result<()> {
    assert_session_asks_before_switching_from_websocket_to_http(SessionSource::Mcp).await
}

#[tokio::test]
async fn cli_can_keep_reconnecting_with_exponential_delays() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_websocket_server(vec![
        vec![
            vec![ev_response_created("prewarm"), ev_completed("prewarm")],
            Vec::new(),
        ],
        vec![Vec::new()],
        vec![vec![
            ev_response_created("recovered"),
            ev_completed("recovered"),
        ]],
    ])
    .await;
    let mut builder = test_codex()
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        });
    let test = builder.build_with_websocket_server(&server).await?;

    start_turn(&test, "keep the websocket transport").await?;
    let first_request = wait_for_fallback_request(&test).await;

    answer_fallback_request(&test, first_request, KEEP_RECONNECTING_LABEL).await?;
    let EventMsg::StreamError(first_retry) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::StreamError(_))
    })
    .await
    else {
        unreachable!("wait_for_event should return a stream error")
    };
    assert_eq!(first_retry.message, "Reconnecting WebSocket in 5s");

    let second_request = wait_for_fallback_request(&test).await;
    answer_fallback_request(&test, second_request, KEEP_RECONNECTING_LABEL).await?;
    let EventMsg::StreamError(second_retry) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::StreamError(_))
    })
    .await
    else {
        unreachable!("wait_for_event should return a stream error")
    };
    assert_eq!(second_retry.message, "Reconnecting WebSocket in 10s");

    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(20),
    )
    .await;

    let connections = server.connections();
    assert_eq!(connections.len(), 3);
    assert_eq!(connections.iter().map(Vec::len).sum::<usize>(), 4);
    server.shutdown().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_after_retries_exhausted() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // Deferred request prewarm is attempted at startup.
    // The first turn then makes 3 websocket stream attempts (initial try + 2 retries),
    // after which fallback activates and the request is replayed over HTTP.
    assert_eq!(websocket_attempts, 4);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_uses_a_separate_http_retry_budget() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let incomplete_sse = responses::sse(vec![serde_json::json!({
        "type": "response.output_item.done",
    })]);
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            incomplete_sse.clone(),
            incomplete_sse,
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // One configured retry allows 2 WebSocket attempts. After fallback, the independent HTTP
    // budget allows 2 retries, so the third HTTP response completes the turn.
    assert_eq!(websocket_attempts, 3);
    assert_eq!(http_attempts, 3);
    assert_eq!(response_mock.requests().len(), 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_hides_first_websocket_retry_stream_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;
    start_turn(&test, "hello").await?;

    let mut stream_error_messages = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(10), test.codex.next_event())
            .await
            .expect("timeout waiting for event")
            .expect("event stream ended unexpectedly")
            .msg;
        match event {
            EventMsg::StreamError(e) => stream_error_messages.push(e.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let expected_stream_errors = if cfg!(debug_assertions) {
        vec!["Reconnecting... 1/2", "Reconnecting... 2/2"]
    } else {
        vec!["Reconnecting... 2/2"]
    };
    assert_eq!(stream_error_messages, expected_stream_errors);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_is_sticky_across_turns() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("first").await?;
    test.submit_turn("second").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // WebSocket attempts all happen on the first turn:
    // 1 deferred request prewarm attempt (startup) + 3 stream attempts
    // (initial try + 2 retries) before fallback.
    // Fallback is sticky, so the second turn stays on HTTP and adds no websocket attempts.
    assert_eq!(websocket_attempts, 4);
    assert_eq!(http_attempts, 2);
    assert_eq!(response_mock.requests().len(), 2);

    Ok(())
}
