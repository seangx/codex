//! Wire-size regressions: image histories can be large while vision token usage stays low.

use super::*;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use pretty_assertions::assert_eq;
use test_case::test_case;

fn screenshot_data_url() -> Result<String> {
    let png = valid_png_data_url()?;
    let mut bytes = BASE64_STANDARD.decode(png.split_once(',').context("PNG data URL")?.1)?;
    // Valid tiny PNG with trailing padding: high transport cost, low visual token estimate.
    bytes.resize(4 * 1024 * 1024, /*value*/ 0);
    Ok(format!(
        "data:image/png;base64,{}",
        BASE64_STANDARD.encode(bytes)
    ))
}

fn compact_response() -> String {
    sse(vec![
        json!({"type":"response.output_item.done", "item": {
            "type":"compaction", "encrypted_content":"IMAGE_BUDGET_SUMMARY"
        }}),
        responses::ev_completed_with_tokens("compact", /*total_tokens*/ 100),
    ])
}

fn budget_test_codex() -> TestCodexBuilder {
    test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_auto_compact_token_limit = Some(1_000_000);
            config.model_context_window = Some(2_000_000);
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
}

#[test_case(false; "live_history")]
#[test_case(true; "resumed_history")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_image_budget_runs_before_replaying_oversized_history(
    resume: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = TestCodexHarness::with_auto_env_builder(budget_test_codex()).await?;
    let image = screenshot_data_url()?;
    let history: Vec<ResponseItem> = serde_json::from_value(json!([
        {"type":"message", "role":"user", "content":[
            {"type":"input_text", "text":"Inspect these older frames"},
            {"type":"input_image", "image_url":image}
        ]},
        {"type":"function_call", "name":"frames", "call_id":"f", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"f", "output":[
            {"type":"input_image", "image_url":image}
        ]},
        {"type":"custom_tool_call", "name":"frames", "call_id":"c", "input":""},
        {"type":"custom_tool_call_output", "call_id":"c", "output":[
            {"type":"input_image", "image_url":image}
        ]}
    ]))?;
    harness.test().codex.inject_response_items(history).await?;
    let resumed = if resume {
        Some(
            budget_test_codex()
                .restart(harness.server(), harness.test())
                .await?,
        )
    } else {
        None
    };
    let test = resumed.as_ref().unwrap_or_else(|| harness.test());
    let mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
            compact_response(),
            sse(vec![responses::ev_completed_with_tokens(
                "answer", /*total_tokens*/ 100,
            )]),
        ],
    )
    .await;
    let fresh_image = valid_png_data_url()?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: "Inspect the new frame after recovery".into(),
                text_elements: Vec::new(),
            },
            UserInput::Image {
                image: ImageReference::Inline {
                    image_url: fresh_image.clone(),
                },
                detail: None,
            },
        ]))
        .await?;
    wait_for_turn_complete(&test.codex).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].inputs_of_type("compaction_trigger").len(), 1);
    assert!(requests[0].body_json().to_string().contains("codex-image:"));
    assert!(!requests[0].body_contains_text("Inspect the new frame after recovery"));
    assert_eq!(
        requests[1].message_input_image_urls("user"),
        vec![fresh_image]
    );
    assert!(requests[1].body_contains_text("Inspect the new frame after recovery"));
    for request in requests {
        assert!(request.body_json().to_string().len() < 1024 * 1024);
    }
    assert_eq!(
        fs::read_dir(test.config.codex_home.join("compacted-images"))?.count(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_image_budget_preserves_small_history_without_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let harness = TestCodexHarness::with_auto_env_builder(budget_test_codex()).await?;
    let image = screenshot_data_url()?;
    harness
        .test()
        .codex
        .inject_response_items(vec![serde_json::from_value(json!({
            "type":"message", "role":"user", "content":[{"type":"input_image", "image_url":image}]
        }))?])
        .await?;
    let mock = mount_sse_once(
        harness.server(),
        sse(vec![responses::ev_completed("answer")]),
    )
    .await;
    harness.test().submit_turn("Continue inspecting").await?;
    let request = mock.single_request();
    assert!(request.inputs_of_type("compaction_trigger").is_empty());
    assert_eq!(request.message_input_image_urls("user"), vec![image]);
    assert!(
        !harness
            .test()
            .config
            .codex_home
            .join("compacted-images")
            .exists()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_image_budget_handles_new_tool_images_mid_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mut builder = budget_test_codex();
    let mut test = builder.build_with_auto_env(&server).await?;
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![test.executor_environment().selection().clone()]),
            dynamic_tools: vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "frames".into(),
                description: "Capture frames".into(),
                input_schema: json!({"type":"object", "properties":{}}),
                defer_loading: false,
            })],
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    test.codex = thread.thread;
    test.session_configured = thread.session_configured;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                responses::ev_function_call("frames", "frames", "{}"),
                responses::ev_completed_with_tokens("tool", /*total_tokens*/ 100),
            ]),
            compact_response(),
            sse(vec![responses::ev_completed_with_tokens(
                "answer", /*total_tokens*/ 100,
            )]),
        ],
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect frames".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::DynamicToolCallRequest(request) => Some(request.clone()),
        _ => None,
    })
    .await;
    let image = screenshot_data_url()?;
    test.codex
        .submit(Op::DynamicToolResponse {
            id: request.call_id,
            response: DynamicToolResponse {
                content_items: (0..3)
                    .map(|_| DynamicToolCallOutputContentItem::InputImage {
                        image_url: image.clone(),
                    })
                    .collect(),
                success: true,
            },
        })
        .await?;
    wait_for_turn_complete(&test.codex).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].inputs_of_type("compaction_trigger").len(), 1);
    assert!(requests[1].body_json().to_string().contains("codex-image:"));
    assert_eq!(
        requests[2].inputs_of_type("compaction")[0]["encrypted_content"],
        "IMAGE_BUDGET_SUMMARY"
    );
    for request in requests {
        assert!(!request.body_json().to_string().contains("data:image/"));
        assert!(request.body_json().to_string().len() < 1024 * 1024);
    }
    Ok(())
}
