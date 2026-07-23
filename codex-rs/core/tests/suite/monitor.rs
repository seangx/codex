#![cfg(not(target_os = "windows"))]

use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::json;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_output_wakes_an_idle_session() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let monitor_args = json!({
        "cmd": "sleep 0.8; printf 'monitor-finished\\n'",
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "monitor-call",
                    "monitor",
                    &serde_json::to_string(&monitor_args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "monitor started"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "monitor event received"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.use_experimental_unified_exec_tool = true;
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("unified exec should enable for test");
        config
            .features
            .enable(Feature::MonitorTool)
            .expect("monitor tool should enable for test");
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("start a monitor").await?;

    timeout(Duration::from_secs(5), async {
        loop {
            if responses.requests().len() >= 3 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("monitor output should wake the model")?;

    let requests = responses.requests();
    let wake_request = requests
        .get(2)
        .context("monitor wake should produce a third model request")?;
    let monitor_events = wake_request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.contains("<monitor_event>"))
        .collect::<Vec<_>>();
    assert_eq!(monitor_events.len(), 1);
    assert!(monitor_events[0].contains("monitor-finished"));
    assert!(monitor_events[0].contains(r#""status":{"exited":{"exit_code":0}}"#));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_stop_terminates_a_running_monitor() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let monitor_args = json!({
        "cmd": "sleep 10",
    });
    let stop_args = json!({
        "session_id": 1000,
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-stop-1"),
                ev_function_call(
                    "monitor-call",
                    "monitor",
                    &serde_json::to_string(&monitor_args)?,
                ),
                ev_completed("resp-stop-1"),
            ]),
            sse(vec![
                ev_response_created("resp-stop-2"),
                ev_function_call(
                    "monitor-stop-call",
                    "monitor_stop",
                    &serde_json::to_string(&stop_args)?,
                ),
                ev_completed("resp-stop-2"),
            ]),
            sse(vec![
                ev_response_created("resp-stop-3"),
                ev_assistant_message("msg-stop-3", "monitor stopped"),
                ev_completed("resp-stop-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.use_experimental_unified_exec_tool = true;
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("unified exec should enable for test");
        config
            .features
            .enable(Feature::MonitorTool)
            .expect("monitor tool should enable for test");
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("start and stop a monitor").await?;

    let requests = responses.requests();
    let stop_request = requests
        .get(2)
        .context("monitor_stop should produce a third model request")?;
    let (output, success) = stop_request
        .function_call_output_content_and_success("monitor-stop-call")
        .context("monitor_stop output should be model visible")?;
    assert_ne!(success, Some(false));
    assert_eq!(output.as_deref(), Some("Stopped monitor 1000."));

    Ok(())
}
