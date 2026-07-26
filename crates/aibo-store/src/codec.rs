//! Mapping between `aibo-core` enums and the TEXT values §12's schema stores.
//!
//! The DDL comments are the specification: `surface` is
//! `complete|transform|ask|do`, `messages.role` is `system|user|assistant|tool`,
//! `clipboard_history.kind` is `text|image_ref|files`. These functions are the
//! single place that mapping lives, so a rename in `aibo-core` breaks
//! compilation here rather than silently writing an unreadable column.

use aibo_core::types::{ClipboardKind, MessageRole, Surface};

use crate::error::{Result, StoreError};

/// `conversations.surface`.
pub fn surface_to_str(surface: Surface) -> &'static str {
    match surface {
        Surface::Complete => "complete",
        Surface::Transform => "transform",
        Surface::Ask => "ask",
        Surface::Do => "do",
    }
}

/// Parse `conversations.surface`.
pub fn surface_from_str(value: &str) -> Result<Surface> {
    match value {
        "complete" => Ok(Surface::Complete),
        "transform" => Ok(Surface::Transform),
        "ask" => Ok(Surface::Ask),
        "do" => Ok(Surface::Do),
        other => Err(StoreError::BadColumn {
            column: "conversations.surface",
            value: other.to_owned(),
        }),
    }
}

/// `messages.role`.
pub fn message_role_to_str(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

/// Parse `messages.role`.
pub fn message_role_from_str(value: &str) -> Result<MessageRole> {
    match value {
        "system" => Ok(MessageRole::System),
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        "tool" => Ok(MessageRole::Tool),
        other => Err(StoreError::BadColumn {
            column: "messages.role",
            value: other.to_owned(),
        }),
    }
}

/// `clipboard_history.kind`.
///
/// Returns `None` for the two kinds that are not payloads at all — an empty
/// clipboard and one holding a type aibo does not handle. Neither has anything
/// worth a history row.
pub fn clipboard_kind_to_str(kind: ClipboardKind) -> Option<&'static str> {
    match kind {
        ClipboardKind::Text => Some("text"),
        ClipboardKind::ImageRef => Some("image_ref"),
        ClipboardKind::Files => Some("files"),
        ClipboardKind::Unsupported | ClipboardKind::Empty => None,
    }
}

/// Parse `clipboard_history.kind`.
pub fn clipboard_kind_from_str(value: &str) -> Result<ClipboardKind> {
    match value {
        "text" => Ok(ClipboardKind::Text),
        "image_ref" => Ok(ClipboardKind::ImageRef),
        "files" => Ok(ClipboardKind::Files),
        other => Err(StoreError::BadColumn {
            column: "clipboard_history.kind",
            value: other.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_round_trips_every_variant() {
        for surface in [
            Surface::Complete,
            Surface::Transform,
            Surface::Ask,
            Surface::Do,
        ] {
            let text = surface_to_str(surface);
            assert_eq!(surface_from_str(text).expect("parse"), surface);
        }
    }

    #[test]
    fn message_role_round_trips_every_variant() {
        for role in [
            MessageRole::System,
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
        ] {
            let text = message_role_to_str(role);
            assert_eq!(message_role_from_str(text).expect("parse"), role);
        }
    }

    #[test]
    fn unknown_values_are_named_rather_than_guessed() {
        let err = surface_from_str("compute").expect_err("Compute is not a routed surface");
        assert!(matches!(err, StoreError::BadColumn { .. }));
    }
}
