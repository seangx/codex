use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

/// A fixed-size reference, not a generated description of an image's contents.
pub(crate) struct ArchivedImageReference {
    pub(crate) digest: String,
}

impl ArchivedImageReference {
    pub(crate) fn body_text(&self) -> String {
        format!(
            "[Image archived locally. To inspect it again, call view_image with path=\"codex-image:{}\". Preserve this reference alongside any existing visual findings in the summary; do not infer details from this placeholder.]",
            self.digest
        )
    }
}

impl ContextualUserFragment for ArchivedImageReference {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("user.image_reference".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> ContentItem {
        input_text(self.body_text())
    }
}
