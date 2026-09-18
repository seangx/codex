use super::ARCHIVE_DIRECTORY;
use super::archive_image;
use super::load_archived_image;
use super::prepare_history_for_compaction;
use crate::context::ContextualUserFragment;
use crate::context_manager::ContextManager;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test]
async fn compaction_snapshot_archives_inline_images_and_preserves_file_references_without_mutating_live_history()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let image = "data:image/png;base64,cGljdHVyZQ==";
    let items: Vec<ResponseItem> = serde_json::from_value(json!([
        {"type":"message", "role":"user", "content":[
            {"type":"input_text", "text":"Inspect the grip"},
            {"type":"input_image", "image_url":image},
            {"type":"input_image", "file_id":"file-user", "detail":"low"}
        ]},
        {"type":"message", "role":"assistant", "content":[
            {"type":"output_text", "text":"The grip is aligned"}
        ]},
        {"type":"function_call_output", "call_id":"f", "output":[
            {"type":"input_image", "image_url":image},
            {"type":"input_image", "file_id":"file-function"}
        ]},
        {"type":"custom_tool_call_output", "call_id":"c", "output":[
            {"type":"input_image", "image_url":image},
            {"type":"input_image", "file_id":"file-custom", "detail":"original"}
        ]}
    ]))?;
    let mut history = ContextManager::default();
    history.replace(items.clone());
    let session_source = SessionSource::Exec;
    let archived =
        prepare_history_for_compaction(history.clone(), home.path().to_path_buf(), &session_source)
            .await?;
    assert_eq!(history.into_raw_items(), items);
    let reference = archive_image(home.path(), image)?;
    let replacement = reference.body();
    let mut expected = serde_json::to_value(items)?;
    expected[0]["content"][1] = json!({"type":"input_text", "text":replacement});
    expected[0]["internal_chat_message_metadata_passthrough"] = json!({
        "content_item_kinds": ["unknown", "user.image_reference", "unknown"]
    });
    for index in [2, 3] {
        expected[index]["output"][0] = json!({"type":"input_text", "text":replacement});
    }
    assert_eq!(
        serde_json::to_value(archived.clone().into_raw_items())?,
        expected
    );
    assert_eq!(
        std::fs::read_dir(home.path().join(ARCHIVE_DIRECTORY))?.count(),
        1
    );
    let reference_path = format!("codex-image:{}", reference.digest);
    assert_eq!(
        load_archived_image(home.path(), &reference_path).await?,
        Some(image.to_string())
    );
    let second = prepare_history_for_compaction(
        archived.clone(),
        home.path().to_path_buf(),
        &session_source,
    )
    .await?;
    assert_eq!(
        second.into_annotated_items(),
        archived.into_annotated_items()
    );
    assert_eq!(
        std::fs::read_dir(home.path().join(ARCHIVE_DIRECTORY))?.count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn archive_failure_leaves_original_history_available() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join(ARCHIVE_DIRECTORY), "not a directory")?;
    let items = serde_json::from_value(json!([
        {"type":"message", "role":"user", "content":[
            {"type":"input_image", "image_url":"data:image/png;base64,cGljdHVyZQ=="}
        ]}
    ]))?;
    let mut history = ContextManager::default();
    history.replace(items);
    let original = history.clone().into_annotated_items();
    let session_source = SessionSource::Exec;
    assert!(
        prepare_history_for_compaction(
            history.clone(),
            home.path().to_path_buf(),
            &session_source,
        )
        .await
        .is_err()
    );
    assert_eq!(history.into_annotated_items(), original);
    Ok(())
}

#[tokio::test]
async fn guardian_compaction_preserves_multimodal_evidence_without_archiving() -> anyhow::Result<()>
{
    let home = tempfile::tempdir()?;
    let item: ResponseItem = serde_json::from_value(json!({
        "type":"message",
        "role":"user",
        "content":[
            {"type":"input_text", "text":"review evidence"},
            {"type":"input_image", "image_url":"data:image/png;base64,cGljdHVyZQ=="}
        ]
    }))?;
    let envelope = ResponseItemEnvelope {
        item,
        metadata: Some(CodexHarnessMetadata {
            history_truncation_token_limit: Some(10_000),
            ..Default::default()
        }),
    };
    let mut history = ContextManager::default();
    history.replace_annotated(vec![envelope.clone()]);
    let session_source = SessionSource::SubAgent(SubAgentSource::Other(
        crate::guardian::GUARDIAN_REVIEWER_NAME.to_string(),
    ));

    let prepared =
        prepare_history_for_compaction(history, home.path().to_path_buf(), &session_source).await?;

    assert_eq!(prepared.into_annotated_items(), vec![envelope]);
    assert!(!home.path().join(ARCHIVE_DIRECTORY).exists());
    Ok(())
}

#[tokio::test]
async fn archive_references_reject_path_traversal_and_corrupt_files() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    for reference in ["codex-image:../file", "codex-image:", "codex-image:abc"] {
        assert!(load_archived_image(home.path(), reference).await.is_err());
    }
    assert_eq!(
        load_archived_image(home.path(), "screenshot.png").await?,
        None
    );
    let reference = archive_image(home.path(), "data:image/png;base64,cGljdHVyZQ==")?;
    let path = home
        .path()
        .join(ARCHIVE_DIRECTORY)
        .join(format!("{}.image", reference.digest));
    std::fs::write(path, "damaged")?;
    assert!(
        load_archived_image(home.path(), &format!("codex-image:{}", reference.digest))
            .await
            .is_err()
    );
    assert!(archive_image(home.path(), "data:image/png;base64,cGljdHVyZQ==").is_err());
    Ok(())
}
