mod streamable_http_test_support;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_exec_server::Environment;
use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::MetaObject;
use serde_json::Value;
use serde_json::json;

use streamable_http_test_support::arm_initialize_post_failure;
use streamable_http_test_support::arm_initialize_post_json_rpc_failure;
use streamable_http_test_support::arm_initialized_notification_post_json_rpc_failure;
use streamable_http_test_support::arm_session_post_failure;
use streamable_http_test_support::arm_session_post_failure_with_body;
use streamable_http_test_support::arm_session_post_json_rpc_failure;
use streamable_http_test_support::call_echo_tool;
use streamable_http_test_support::create_client;
use streamable_http_test_support::create_client_with_http_client;
use streamable_http_test_support::expected_echo_result;
use streamable_http_test_support::spawn_streamable_http_server;

const JSON_RPC_INTERNAL_ERROR_CODE: i64 = -32603;
const SIMULATED_NO_RESPONSE_MESSAGE: &str =
    "http/request failed: error sending request for url (simulated no response)";

#[derive(Clone)]
struct FailFirstInitializeHttpClient {
    inner: Arc<dyn HttpClient>,
    failures_remaining: Arc<AtomicUsize>,
    initialize_attempts: Arc<AtomicUsize>,
}

impl FailFirstInitializeHttpClient {
    fn new(inner: Arc<dyn HttpClient>, failures_remaining: usize) -> Self {
        Self {
            inner,
            failures_remaining: Arc::new(AtomicUsize::new(failures_remaining)),
            initialize_attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn initialize_attempts(&self) -> usize {
        self.initialize_attempts.load(Ordering::SeqCst)
    }

    fn fail_next_initialize(&self) {
        self.failures_remaining.store(1, Ordering::SeqCst);
    }
}

impl HttpClient for FailFirstInitializeHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        self.inner.http_request(params)
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        let inner = Arc::clone(&self.inner);
        let failures_remaining = Arc::clone(&self.failures_remaining);
        let initialize_attempts = Arc::clone(&self.initialize_attempts);

        async move {
            if is_initialize_post(&params) {
                initialize_attempts.fetch_add(1, Ordering::SeqCst);
                if failures_remaining.swap(0, Ordering::SeqCst) > 0 {
                    return Err(ExecServerError::Server {
                        code: JSON_RPC_INTERNAL_ERROR_CODE,
                        message: SIMULATED_NO_RESPONSE_MESSAGE.to_string(),
                    });
                }
            }

            inner.http_request_stream(params).await
        }
        .boxed()
    }
}

fn is_initialize_post(params: &HttpRequestParams) -> bool {
    params.method.eq_ignore_ascii_case("POST")
        && params
            .body
            .as_ref()
            .and_then(|body| serde_json::from_slice::<Value>(&body.0).ok())
            .and_then(|body| {
                body.get("method")
                    .and_then(Value::as_str)
                    .map(|method| method == "initialize")
            })
            .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_initialize_retries_remote_no_response_error() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 1,
    );

    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;
    let result = call_echo_tool(&client, "after-init-retry").await?;

    assert_eq!(http_client.initialize_attempts(), 2);
    assert_eq!(result, expected_echo_result("after-init-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_initialize_retries_transient_http_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;

    arm_initialize_post_failure(&base_url, /*status*/ 502, /*remaining*/ 1).await?;

    let client = create_client(&base_url).await?;
    let result = call_echo_tool(&client, "after-status-retry").await?;

    assert_eq!(result, expected_echo_result("after-status-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_initialize_retries_json_rpc_transient_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;

    arm_initialize_post_json_rpc_failure(&base_url, /*status*/ 502, /*remaining*/ 1).await?;

    let client = create_client(&base_url).await?;
    let result = call_echo_tool(&client, "after-json-status-retry").await?;

    assert_eq!(result, expected_echo_result("after-json-status-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_retries_initialized_notification_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;

    arm_initialized_notification_post_json_rpc_failure(
        &base_url, /*status*/ 502, /*remaining*/ 1,
    )
    .await?;

    let client = create_client(&base_url).await?;
    let result = call_echo_tool(&client, "after-notification-status-retry").await?;

    assert_eq!(
        result,
        expected_echo_result("after-notification-status-retry")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_tools_list_retries_transient_http_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let expected = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;
    arm_session_post_failure(
        &base_url,
        /*status*/ 502,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let result = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;

    assert_eq!(result, expected);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_tools_list_retries_json_rpc_transient_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let expected = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;
    arm_session_post_json_rpc_failure(&base_url, /*status*/ 502, /*remaining*/ 1).await?;

    let result = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;

    assert_eq!(result, expected);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_404_session_expiry_recovers_and_retries_once() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let recovered = call_echo_tool(&client, "recovered").await?;
    assert_eq!(recovered, expected_echo_result("recovered"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_session_recovery_retries_initialize_failure() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 0,
    );
    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;
    http_client.fail_next_initialize();

    let recovered = call_echo_tool(&client, "recovered-after-retry").await?;
    assert_eq!(http_client.initialize_attempts(), 3);
    assert_eq!(recovered, expected_echo_result("recovered-after-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_401_does_not_trigger_recovery() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 401,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let first_error = call_echo_tool(&client, "unauthorized").await.unwrap_err();
    assert!(first_error.to_string().contains("401"));

    let second_error = call_echo_tool(&client, "still-unauthorized")
        .await
        .unwrap_err();
    assert!(second_error.to_string().contains("401"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_401_challenge_is_a_tool_error_without_replay() -> anyhow::Result<()> {
    let challenge = r#"Bearer error="invalid_token", resource_metadata="https://example.com/.well-known/oauth-protected-resource""#;
    for challenges in [
        vec![challenge],
        vec![r#"Basic realm="proxy, login""#, challenge],
    ] {
        let (_server, base_url) = spawn_streamable_http_server().await?;
        let client = create_client(&base_url).await?;

        arm_session_post_failure(
            &base_url,
            /*status*/ 401,
            /*remaining*/ 1,
            &challenges,
        )
        .await?;

        let result = call_echo_tool(&client, "rejected").await?;
        let mut expected =
            CallToolResult::error(vec![ContentBlock::text("Authentication required")]);
        expected.meta = Some(MetaObject::from(serde_json::Map::from_iter([(
            "mcp/www_authenticate".to_string(),
            json!([challenges.join(", ")]),
        )])));
        assert_eq!(result, expected);

        // A retry inside the failed call would consume the single rejection and return success.
        assert_eq!(
            call_echo_tool(&client, "next-user-call").await?,
            expected_echo_result("next-user-call"),
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_403_scope_challenge_returns_insufficient_scope() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 403,
        /*remaining*/ 1,
        /*www_authenticate_headers*/
        &[r#"Bearer error="insufficient_scope", scope="files:read files:write""#],
    )
    .await?;

    let error = call_echo_tool(&client, "forbidden").await.unwrap_err();
    assert!(
        error.to_string().contains("Insufficient scope"),
        "expected insufficient-scope transport error, got: {error:#}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_403_finds_bearer_challenge_in_later_header_value() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 403,
        /*remaining*/ 1,
        /*www_authenticate_headers*/
        &[
            r#"Basic realm="example""#,
            r#"Bearer error="insufficient_scope", scope="files:read""#,
        ],
    )
    .await?;

    let error = call_echo_tool(&client, "forbidden").await.unwrap_err();
    assert!(
        error.to_string().contains("Insufficient scope"),
        "expected insufficient-scope transport error, got: {error:#}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_404_recovery_only_retries_once() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let error = call_echo_tool(&client, "double-404").await.unwrap_err();
    let error_message = error.to_string();
    assert!(
        error_message.contains("404") || error_message.contains("session expired"),
        "expected session-expiry error, got: {error:#}"
    );

    let recovered = call_echo_tool(&client, "after-double-404").await?;
    assert_eq!(recovered, expected_echo_result("after-double-404"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_invalid_session_400_recovers_and_retries_once() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 0,
    );
    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;

    arm_session_post_failure_with_body(
        &base_url,
        /*status*/ 400,
        /*remaining*/ 1,
        "text/plain",
        "Bad Request: No valid session ID provided",
    )
    .await?;

    let recovered = call_echo_tool(&client, "recovered-after-invalid-session").await?;

    assert_eq!(http_client.initialize_attempts(), 2);
    assert_eq!(
        recovered,
        expected_echo_result("recovered-after-invalid-session")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_invalid_session_400_recovery_only_retries_once() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 0,
    );
    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;

    arm_session_post_failure_with_body(
        &base_url,
        /*status*/ 400,
        /*remaining*/ 2,
        "text/plain",
        "Bad Request: No valid session ID provided",
    )
    .await?;

    let error = call_echo_tool(&client, "still-invalid-session")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("400 Bad Request"),
        "expected invalid-session error, got: {error:#}"
    );
    assert_eq!(http_client.initialize_attempts(), 2);

    assert_eq!(
        call_echo_tool(&client, "after-invalid-session-error").await?,
        expected_echo_result("after-invalid-session-error")
    );
    assert_eq!(http_client.initialize_attempts(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_other_400_bodies_do_not_trigger_recovery_or_replay() -> anyhow::Result<()>
{
    for (content_type, body, expected_error) in [
        ("text/plain", "Bad Request", "HTTP 400"),
        (
            "text/plain",
            "Bad Request: No valid session ID provided.",
            "HTTP 400",
        ),
        (
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"Bad Request: No valid session ID provided"}}"#,
            "Bad Request: No valid session ID provided",
        ),
    ] {
        let (_server, base_url) = spawn_streamable_http_server().await?;
        let http_client = FailFirstInitializeHttpClient::new(
            Environment::default_for_tests().get_http_client(),
            /*failures_remaining*/ 0,
        );
        let client =
            create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;

        arm_session_post_failure_with_body(
            &base_url,
            /*status*/ 400,
            /*remaining*/ 1,
            content_type,
            body,
        )
        .await?;

        let error = call_echo_tool(&client, "must-not-replay")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(expected_error),
            "expected ordinary HTTP 400 error for {body:?}, got: {error:#}"
        );
        assert_eq!(http_client.initialize_attempts(), 1);

        assert_eq!(
            call_echo_tool(&client, "next-user-call").await?,
            expected_echo_result("next-user-call")
        );
        assert_eq!(http_client.initialize_attempts(), 1);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_non_session_failure_does_not_trigger_recovery() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 500,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let first_error = call_echo_tool(&client, "server-error").await.unwrap_err();
    assert!(first_error.to_string().contains("500"));

    let second_error = call_echo_tool(&client, "still-server-error")
        .await
        .unwrap_err();
    assert!(second_error.to_string().contains("500"));

    Ok(())
}
