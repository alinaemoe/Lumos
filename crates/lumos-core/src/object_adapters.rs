//! Translation layer: existing entities → the domain `Object` model.
//!
//! This module is deliberately *not* part of `object.rs` — the Object
//! model itself must stay ignorant of `search`/`clipboard`'s concrete DTOs
//! (that dependency would point the wrong way, see
//! `ARCHITECTURE_INVARIANTS.md` §6). Adapters are the seam that's allowed
//! to know about both sides.
//!
//! Nothing here changes what `search.rs`/`clipboard.rs` do today — these
//! are pure `From` conversions callers can opt into later (Phase 4+); no
//! existing call site is wired to them yet.

use crate::clipboard::ClipboardResult;
use crate::object::{ClipboardPayload, FilePayload, Object, ObjectMetadata, ObjectPayload};
use crate::search::SearchResult;

impl From<&SearchResult> for Object {
    /// `search::SearchResult` doesn't project `created_at`/`modified_at`
    /// or `project_id` (only `project_name`) — those fields land in
    /// `ObjectMetadata` as `None` rather than being guessed at. Extending
    /// what's queried is a Phase 2/4 concern, not something to smuggle in
    /// here.
    fn from(r: &SearchResult) -> Self {
        Object::new(
            r.id,
            ObjectMetadata::default(),
            ObjectPayload::File(FilePayload {
                path: r.path.clone(),
                filename: r.filename.clone(),
                extension: r.extension.clone(),
                size: r.size,
                project_name: r.project_name.clone(),
            }),
        )
    }
}

impl From<SearchResult> for Object {
    fn from(r: SearchResult) -> Self {
        Object::from(&r)
    }
}

impl From<&ClipboardResult> for Object {
    /// Clipboard entries carry no project association today — `project_id`
    /// stays `None`, same reasoning as the file adapter above.
    fn from(c: &ClipboardResult) -> Self {
        Object::new(
            c.id,
            ObjectMetadata {
                created_at: Some(c.created_at),
                updated_at: None,
                project_id: None,
                source: c.source_app.clone(),
            },
            ObjectPayload::Clipboard(ClipboardPayload {
                text_content: c.text_content.clone(),
                detected_type: c.detected_type.clone(),
                pinned: c.pinned,
            }),
        )
    }
}

impl From<ClipboardResult> for Object {
    fn from(c: ClipboardResult) -> Self {
        Object::from(&c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{ObjectPayload, ObjectType};

    #[test]
    fn file_search_result_maps_to_object() {
        let r = SearchResult {
            id: 42,
            path: "C:\\proj\\main.rs".into(),
            filename: "main.rs".into(),
            extension: Some("rs".into()),
            size: 1024,
            project_name: Some("Lumos".into()),
        };
        let obj: Object = (&r).into();
        assert_eq!(obj.id.object_type, ObjectType::File);
        assert_eq!(obj.id.local_id, 42);
        match obj.payload {
            ObjectPayload::File(f) => {
                assert_eq!(f.filename, "main.rs");
                assert_eq!(f.project_name, Some("Lumos".into()));
            }
            _ => panic!("expected File payload"),
        }
    }

    #[test]
    fn clipboard_result_maps_to_object() {
        let c = ClipboardResult {
            id: 7,
            text_content: "SELECT 1".into(),
            detected_type: "sql".into(),
            source_app: Some("VS Code".into()),
            pinned: true,
            created_at: 1_700_000_000,
        };
        let obj: Object = (&c).into();
        assert_eq!(obj.id.object_type, ObjectType::Clipboard);
        assert_eq!(obj.id.local_id, 7);
        assert_eq!(obj.metadata.created_at, Some(1_700_000_000));
        assert_eq!(obj.metadata.source, Some("VS Code".into()));
        match obj.payload {
            ObjectPayload::Clipboard(cp) => {
                assert!(cp.pinned);
                assert_eq!(cp.detected_type, "sql");
            }
            _ => panic!("expected Clipboard payload"),
        }
    }
}
