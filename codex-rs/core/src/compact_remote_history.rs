use std::borrow::Borrow;

use crate::context::ContextualUserFragment;
use crate::context::ImageResizeNotice;
use crate::context_manager::estimate_item_token_count;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoryItemGroup<T> {
    pub(crate) source: T,
    pub(crate) attached_notice: Option<T>,
}

impl<T: Borrow<ResponseItem>> HistoryItemGroup<T> {
    pub(crate) fn into_items(self) -> impl Iterator<Item = T> {
        std::iter::once(self.source).chain(self.attached_notice)
    }

    pub(crate) fn estimated_token_count(&self) -> i128 {
        let source_tokens = i128::from(estimate_item_token_count(self.source.borrow()));
        let notice_tokens = self.attached_notice.as_ref().map_or(0, |notice| {
            i128::from(estimate_item_token_count(notice.borrow()))
        });
        source_tokens.saturating_add(notice_tokens)
    }
}

pub(crate) fn history_item_groups<I>(items: I) -> impl Iterator<Item = HistoryItemGroup<I::Item>>
where
    I: IntoIterator,
    I::Item: Borrow<ResponseItem>,
{
    let mut items = items.into_iter().peekable();
    std::iter::from_fn(move || {
        let source = items.next()?;
        let attached_notice = items.next_if(|notice| is_attached_notice(notice.borrow()));
        Some(HistoryItemGroup {
            source,
            attached_notice,
        })
    })
}

/// Keeps the newest complete history groups within an item-count limit.
///
/// The grouping keeps generated resize notices attached to their source item, so a stable tail
/// slice never begins with a detached notice. Items remain in their original order.
pub(crate) fn retain_recent_history_item_groups<T>(items: Vec<T>, max_items: usize) -> Vec<T>
where
    T: Borrow<ResponseItem>,
{
    if items.len() <= max_items {
        return items;
    }

    let mut remaining = max_items;
    let mut retained_reversed = Vec::new();
    for group in history_item_groups(items)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let group_len = 1 + usize::from(group.attached_notice.is_some());
        if group_len > remaining {
            break;
        }
        remaining -= group_len;
        retained_reversed.push(group);
    }
    retained_reversed.reverse();
    retained_reversed
        .into_iter()
        .flat_map(HistoryItemGroup::into_items)
        .collect()
}

fn is_attached_notice(notice: &ResponseItem) -> bool {
    matches!(
        notice,
        ResponseItem::Message { role, content, .. }
            if role == "developer"
                && matches!(
                    content.as_slice(),
                    [ContentItem::InputText { text }]
                        if ImageResizeNotice::matches_text(text)
                )
    )
}
