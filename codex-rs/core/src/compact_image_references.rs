//! Image lifetime changes belong to a compaction snapshot, never the normal request path.
//! The original rollout and source files remain untouched. Content-addressed, host-local
//! archives are read through `view_image` references, including with a remote executor.

use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_context_fragments::AnnotatedContent;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use sha1::Digest;
use sha1::Sha1;

use crate::context::ArchivedImageReference;
use crate::context::ContextualUserFragment;
use crate::context::ImageResizeNotice;
use crate::context_manager::ContextManager;
use crate::context_manager::clone_envelope_for_compaction_prompt;

const REFERENCE_PREFIX: &str = "codex-image:";
const ARCHIVE_DIRECTORY: &str = "compacted-images";
// A proactive history budget, not a provider request-size limit. Vision token estimates do not
// reflect base64 wire size; compact well before repeated screenshots grow into a huge replay.
const INLINE_IMAGE_HISTORY_BUDGET: usize = 16 * 1024 * 1024;
// A defensive file-read bound, not a resizing or quality policy. Prompt images already have
// a 1 GiB decoded input limit; allow its base64 representation and MIME header here.
const MAX_ARCHIVE_BYTES: u64 = 1_500_000_000;

/// Check borrowed history without serializing or cloning image payloads. This is independent of
/// the token budget and also applies to histories restored from disk. Guardian evidence is not
/// archived by compaction, so it must keep using its own admission limits instead.
pub(crate) fn image_history_budget_reached(
    history: &ContextManager,
    session_source: &SessionSource,
) -> bool {
    if crate::guardian::is_basic_session_source(session_source) {
        return false;
    }
    let inline_bytes = |image: &ImageReference| match image {
        ImageReference::Inline { image_url } => image_url.len(),
        ImageReference::File { .. } => 0,
    };
    let mut bytes = 0usize;
    for envelope in history.annotated_items() {
        let item_bytes = match &envelope.item {
            ResponseItem::Message { content, .. } => content.iter().fold(0usize, |sum, item| {
                sum.saturating_add(match item {
                    ContentItem::InputImage { image, .. } => inline_bytes(image),
                    ContentItem::InputText { .. }
                    | ContentItem::OutputText { .. }
                    | ContentItem::InputAudio { .. } => 0,
                })
            }),
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => match &output.body {
                FunctionCallOutputBody::ContentItems(items) => {
                    items.iter().fold(0usize, |sum, item| {
                        sum.saturating_add(match item {
                            FunctionCallOutputContentItem::InputImage { image, .. } => {
                                inline_bytes(image)
                            }
                            FunctionCallOutputContentItem::InputText { .. }
                            | FunctionCallOutputContentItem::InputAudio { .. }
                            | FunctionCallOutputContentItem::EncryptedContent { .. } => 0,
                        })
                    })
                }
                FunctionCallOutputBody::Text(_) => 0,
            },
            ResponseItem::ImageGenerationCall { result, .. } => result.len(),
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => 0,
        };
        bytes = bytes.saturating_add(item_bytes);
        if bytes >= INLINE_IMAGE_HISTORY_BUDGET {
            tracing::info!(
                inline_image_bytes = bytes,
                inline_image_budget_bytes = INLINE_IMAGE_HISTORY_BUDGET,
                "inline image history reached compaction budget"
            );
            return true;
        }
    }
    false
}

pub(crate) async fn prepare_history_for_compaction(
    mut history: ContextManager,
    codex_home: PathBuf,
    session_source: &SessionSource,
) -> Result<ContextManager> {
    // Guardian input images are bounded, explicitly admitted review evidence. Keep them directly
    // model-visible across the reviewer's own compaction rather than replacing them with archive
    // handles that the classifier would need a tool call to inspect.
    if crate::guardian::is_basic_session_source(session_source) {
        return Ok(history);
    }

    tokio::task::spawn_blocking(move || -> Result<ContextManager> {
        // Borrow image strings while archiving instead of copying all inline payloads.
        let mut items = Vec::with_capacity(history.annotated_items().len());
        for source in history.annotated_items() {
            let mut envelope = clone_envelope_for_compaction_prompt(source);
            let has_image_content = matches!(&source.item, ResponseItem::Message { content, .. }
            if content.iter().any(|item| match item {
                ContentItem::InputImage { .. } => true,
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    ImageResizeNotice::matches_text(text)
                }
                ContentItem::InputAudio { .. } => true,
            }));
            if has_image_content
                && let ResponseItem::Message {
                    content: original, ..
                } = &source.item
                && let Some(mut content) = to_annotated_content(&mut envelope.item)
            {
                for (item, original) in content.iter_mut().zip(original) {
                    if let ContentItem::InputImage { image, .. } = original {
                        match image {
                            ImageReference::Inline { image_url } => {
                                let reference = archive_image(&codex_home, image_url)?;
                                *item = AnnotatedContent::input_text(
                                    reference.body(),
                                    reference.content_kind(),
                                );
                            }
                            ImageReference::File { .. } => {
                                // Provider-backed references remain model-visible so the retained
                                // image budget can decide whether to keep them after compaction.
                                *item =
                                    AnnotatedContent::new(original.clone(), item.kind().clone());
                            }
                        }
                    } else if matches!(original, ContentItem::InputAudio { .. }) {
                        // Image archival does not change audio retention.
                        *item = AnnotatedContent::new(original.clone(), item.kind().clone());
                    }
                }
                // The preceding images are now references, not resized model-visible images.
                content.retain(|item| {
                    item.kind().0 != "images.resize_notice"
                        && !matches!(item.content(), ContentItem::InputText { text }
                            | ContentItem::OutputText { text }
                            if ImageResizeNotice::matches_text(text))
                });
                let _ = set_annotated_content(&mut envelope.item, content);
            }
            match (&source.item, &mut envelope.item) {
                (
                    ResponseItem::FunctionCallOutput {
                        output: original, ..
                    },
                    ResponseItem::FunctionCallOutput { output, .. },
                )
                | (
                    ResponseItem::CustomToolCallOutput {
                        output: original, ..
                    },
                    ResponseItem::CustomToolCallOutput { output, .. },
                ) => {
                    if let (
                        FunctionCallOutputBody::ContentItems(original),
                        FunctionCallOutputBody::ContentItems(content),
                    ) = (&original.body, &mut output.body)
                    {
                        for (item, original) in content.iter_mut().zip(original) {
                            if let FunctionCallOutputContentItem::InputImage { image, .. } =
                                original
                            {
                                match image {
                                    ImageReference::Inline { image_url } => {
                                        *item = FunctionCallOutputContentItem::InputText {
                                            text: archive_image(&codex_home, image_url)?.body(),
                                        };
                                    }
                                    ImageReference::File { .. } => *item = original.clone(),
                                }
                            } else if matches!(
                                original,
                                FunctionCallOutputContentItem::InputAudio { .. }
                            ) {
                                *item = original.clone();
                            }
                        }
                    }
                }
                (
                    ResponseItem::ImageGenerationCall { result, .. },
                    ResponseItem::ImageGenerationCall { revised_prompt, .. },
                ) if !result.is_empty() => {
                    let reference = archive_image(
                        &codex_home,
                        &format!("data:application/octet-stream;base64,{result}"),
                    )?;
                    let prompt = revised_prompt.get_or_insert_default();
                    prompt.push('\n');
                    prompt.push_str(&reference.body());
                }
                _ => {}
            }
            items.push(envelope);
        }
        items.retain(|envelope| {
            !matches!(&envelope.item,
            ResponseItem::Message { content, .. } if content.is_empty())
        });
        // Preserve authorization and retained-context bookkeeping on this isolated snapshot.
        history.replace_compacted(items, /*reviewer_compaction_hash*/ None);
        Ok(history)
    })
    .await
    .map_err(io::Error::other)?
}

fn archive_image(codex_home: &Path, image_url: &str) -> io::Result<ArchivedImageReference> {
    if image_url.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(io::Error::other(
            "image exceeds the archive file size limit",
        ));
    }
    let digest = format!("{:x}", Sha1::digest(image_url.as_bytes()));
    let directory = codex_home.join(ARCHIVE_DIRECTORY);
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{digest}.image"));
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            // Never overwrite or follow a damaged archive, including hash collisions.
            if read_archive(&path, &digest)? != image_url {
                return Err(io::Error::other("image archive content mismatch"));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
            temporary.write_all(image_url.as_bytes())?;
            temporary.as_file().sync_all()?;
            match temporary.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    if read_archive(&path, &digest)? != image_url {
                        return Err(io::Error::other("image archive content mismatch"));
                    }
                }
                Err(error) => return Err(error.error),
            }
        }
        Err(error) => return Err(error),
    }
    Ok(ArchivedImageReference { digest })
}

/// Only archive handles bypass executor path resolution. Arbitrary paths continue through
/// the selected environment's filesystem and sandbox in `view_image`.
pub(crate) async fn load_archived_image(
    codex_home: &Path,
    reference: &str,
) -> io::Result<Option<String>> {
    let Some(digest) = reference.strip_prefix(REFERENCE_PREFIX) else {
        return Ok(None);
    };
    if digest.len() != 40
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::other("invalid archived image reference"));
    }
    let path = codex_home
        .join(ARCHIVE_DIRECTORY)
        .join(format!("{digest}.image"));
    let digest = digest.to_string();
    tokio::task::spawn_blocking(move || read_archive(&path, &digest).map(Some))
        .await
        .map_err(io::Error::other)?
}

fn read_archive(path: &Path, digest: &str) -> io::Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_ARCHIVE_BYTES {
        return Err(io::Error::other("invalid image archive file"));
    }
    let image_url = fs::read_to_string(path)?;
    if format!("{:x}", Sha1::digest(image_url.as_bytes())) != digest {
        return Err(io::Error::other("image archive content mismatch"));
    }
    Ok(image_url)
}

#[cfg(test)]
#[path = "compact_image_references_tests.rs"]
mod tests;
