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
use crate::models::FileRecord;
use crate::object::{ClipboardPayload, FilePayload, Object, ObjectMetadata, ObjectPayload};
use crate::search::SearchResult;

/// `FileRecord` → `Object`.
///
/// Not a `From` impl, because `FileRecord` carries no identity of its own —
/// it models the *columns* of a file row, not the row itself, so the caller
/// has to supply the storage-assigned id. Making this a `From` would force
/// a fabricated id (0, or an `Option` nobody checks), which is exactly the
/// kind of quiet lie this model exists to prevent.
///
/// This is the richest of the three adapters: unlike `SearchResult`,
/// `FileRecord` actually carries `project_id`, `created_at`, `modified_at`
/// and `hash`, so the resulting `ObjectMetadata` is fully populated.
pub fn file_record_to_object(id: i64, r: &FileRecord) -> Object {
    Object::new(
        id,
        ObjectMetadata {
            created_at: Some(r.created_at),
            updated_at: Some(r.modified_at),
            project_id: r.project_id,
            source: None,
        },
        ObjectPayload::File(FilePayload {
            path: r.path.clone(),
            filename: r.filename.clone(),
            extension: r.extension.clone(),
            size: r.size,
            hash: r.hash.clone(),
            // FileRecord stores the project by id, not by name — resolving
            // it to a display name is a storage-layer join, not something
            // an adapter should invent.
            project_name: None,
        }),
    )
}

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
                hash: None,
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
                // `ClipboardResult` comes from a read that doesn't project
                // `search_extra`, so it is genuinely unknown here — not
                // empty. Same honesty rule as the file adapter's metadata.
                search_extra: None,
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
    fn file_record_maps_to_object_with_full_metadata() {
        let r = FileRecord {
            path: "C:\\proj\\lib.rs".into(),
            filename: "lib.rs".into(),
            extension: Some("rs".into()),
            size: 2048,
            hash: Some("abc123".into()),
            file_type: None,
            project_id: Some(3),
            created_at: 1_700_000_000,
            modified_at: 1_700_000_500,
        };
        let obj = file_record_to_object(99, &r);
        assert_eq!(obj.id.object_type, ObjectType::File);
        assert_eq!(obj.id.local_id, 99);
        // Unlike the SearchResult adapter, this one has real metadata to map.
        assert_eq!(obj.metadata.created_at, Some(1_700_000_000));
        assert_eq!(obj.metadata.updated_at, Some(1_700_000_500));
        assert_eq!(obj.metadata.project_id, Some(3));
        match obj.payload {
            ObjectPayload::File(f) => {
                assert_eq!(f.hash, Some("abc123".into()));
                assert_eq!(f.project_name, None);
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
