//! The shared ingestion lifecycle — see `ARCHITECTURE_INVARIANTS.md` §2.
//!
//! # The seven stages, and where each one actually lives
//!
//! ```text
//! Raw Event → Watch → Parse → Normalize → Classify → Embed → Store → Index
//! ```
//!
//! Every source implements these stages. Most of them are *genuinely*
//! source-specific — a file's `Parse` (stat + read bytes) and a clipboard
//! entry's `Parse` (read the OS clipboard buffer) have nothing in common,
//! and forcing them through one function would just relocate the
//! `match FileEvent | ClipboardEvent` this architecture exists to avoid.
//! What *is* shared is documented below and lives in [`ingest`] — the one
//! function every source funnels through once it has produced a
//! [`NewObject`].
//!
//! | Stage | File (today) | Clipboard (today) | Shared? |
//! |---|---|---|---|
//! | Watch | `scan::scan_path` (`WalkDir`) / `watcher::watch_and_index` (`notify`) | `clipboard_watcher::spawn` (poll loop, in `apps/lumos-app`) | No — inherently OS/source-specific (`ARCHITECTURE_INVARIANTS.md` §2.3) |
//! | Parse | `std::fs::metadata`, `indexer::hash_file`, filename/extension split | Reading the OS clipboard buffer (outside this crate — see below) | No |
//! | Normalize | Assembling `FilePayload` + `ObjectMetadata` in `indexer::index_file` | `clipboard::truncate_utf8` + assembling `ClipboardPayload` | No — different shapes in, same shape (`NewObject`) out |
//! | Classify | `metadata::resolve_project_for_path` (project association *is* classification here) | `clipboard::classify` + `clipboard::build_search_extra` | No |
//! | Embed | `embeddings::store` called on the filename | `embeddings::store` called on the entry text | **Yes** — [`ingest`] calls it uniformly |
//! | Store | `ObjectStore::save` | `ObjectStore::save` | **Yes** — was already shared as of Phase 2.1 |
//! | Index | FTS5 sync trigger on the `files`/`clipboard_entries` table | same | **Yes**, and inseparable from Store — see below |
//!
//! Before this module, the "shared" rows above (Embed, Store) were each
//! duplicated by hand at the end of `index_file` and `record_text`:
//! identical two-line "save, then best-effort embed, swallow errors"
//! sequences with an identical comment explaining why. That duplication —
//! not the source-specific stages above it — is what this module removes.
//!
//! # Where `clipboard`'s Watch/Parse actually live
//!
//! Unlike files, clipboard's Watch stage (poll loop) and the first half of
//! Parse (reading the OS clipboard buffer) live in `apps/lumos-app`, not in
//! `lumos-core`. `clipboard::record_text` receives already-read text — its
//! own "Parse" is really just truncation. This asymmetry against
//! `watcher.rs` (which *is* in core) predates this phase and is not fixed
//! here: moving clipboard polling into `lumos-core` is a real structural
//! change, not something this phase's scope ("unify the Store/Embed tail,
//! document the stages") covers. Left as a known deviation, not hidden.
//!
//! # Store and Index are conceptually distinct, physically fused
//!
//! `ObjectStore::save` issues one `INSERT`/`UPDATE` per object. The
//! `files_fts`/`clipboard_fts` virtual tables are kept in sync by SQL
//! triggers (`files_ai`/`files_au`/`files_ad` etc., see `db.rs`) that fire
//! as part of that same statement, inside SQLite, atomically. There is no
//! separate "now index it" call to make, and inventing an `IndexStore`
//! trait with a single implementation that just re-delegates to the same
//! `INSERT` would be exactly the kind of speculative abstraction this
//! phase is told not to add. The distinction is real (Store = "this object
//! exists"; Index = "this object is findable") and worth keeping in mind
//! for a future source whose search backing *isn't* SQLite FTS — but
//! nothing in the current architecture needs it split into two calls.
//!
//! # Where batching will belong
//!
//! `scan_path` calls `index_file` once per file, and each `ObjectStore::save`
//! runs in its own implicit SQLite transaction (autocommit) — a known
//! bottleneck for a 2M-file scan, and *the* bottleneck for a hypothetical
//! 100K-row browser-history import. This phase does not fix it, per the
//! brief, but the boundary is worth naming precisely: `SqliteStore` borrows
//! `&Connection` and does not open transactions of its own, so a bulk
//! importer that holds the concrete `Connection` can already wrap a whole
//! batch in one `conn.transaction()` around repeated `store.save()` calls
//! today, with zero change to `ObjectStore`. What doesn't exist yet — and
//! would need adding when a real bulk source shows up — is a way to do that
//! from *generic* pipeline code (`scan_path` is generic over `S:
//! ObjectStore`, not over a concrete `Connection`, so it cannot open a
//! transaction itself). That would most naturally become a
//! `ObjectStore::save_many(&[NewObject]) -> Result<Vec<ObjectId>>` method,
//! added when a source that actually needs it exists — not speculatively
//! here.

use crate::object::ObjectId;
use crate::storage::{EmbeddingStore, NewObject};
use anyhow::Result;

/// A [`NewObject`] that has finished Classify, paired with the text (if
/// any) that Embed should run on. This is the boundary every source's
/// pipeline produces and [`ingest`] consumes — the point past which no
/// source-specific code runs.
pub struct Ingestable {
    pub object: NewObject,
    /// `None` means this object has nothing meaningful to embed (rare for
    /// today's two sources, but real for e.g. a `Session` object that's
    /// pure metadata). Distinct from "embedding failed" — that case is
    /// handled inside `ingest` itself and never reaches the caller.
    pub embed_text: Option<String>,
}

impl Ingestable {
    pub fn new(object: NewObject, embed_text: Option<String>) -> Self {
        Self { object, embed_text }
    }
}

/// The shared Store→Embed tail. Every source calls this once, after its own
/// Watch/Parse/Normalize/Classify has produced an [`Ingestable`].
///
/// No `match` on object type, no knowledge of SQLite, no knowledge of
/// `files`/`clipboard_entries` — those are exactly the properties
/// `ARCHITECTURE_INVARIANTS.md` §4/§6 require of shared code, and the
/// reason this function takes `S: ObjectStore + EmbeddingStore` rather than
/// a concrete store.
///
/// Embedding failure — including "no model available at all" — never fails
/// ingestion: the object is already saved by the time `Embed` runs, and a
/// missing vector degrades semantic search, not storage
/// (`ARCHITECTURE_INVARIANTS.md` §8).
pub fn ingest<S>(store: &S, item: Ingestable) -> Result<ObjectId>
where
    S: crate::storage::ObjectStore + EmbeddingStore,
{
    let id = store.save(&item.object)?;
    if let Some(text) = item.embed_text {
        let _ = crate::embeddings::store(store, id, &text);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{ClipboardPayload, FilePayload, ObjectMetadata, ObjectPayload, ObjectType};
    use crate::storage_sqlite::SqliteStore;

    fn store_conn() -> rusqlite::Connection {
        crate::db::open_in_memory().expect("in-memory schema")
    }

    fn file_ingestable(path: &str) -> Ingestable {
        Ingestable::new(
            NewObject::new(
                ObjectMetadata { created_at: Some(1), updated_at: Some(1), project_id: None, source: None },
                ObjectPayload::File(FilePayload {
                    path: path.into(),
                    filename: "main.rs".into(),
                    extension: Some("rs".into()),
                    size: 10,
                    hash: None,
                    project_name: None,
                }),
            ),
            Some("main.rs".into()),
        )
    }

    fn clipboard_ingestable(text: &str) -> Ingestable {
        Ingestable::new(
            NewObject::new(
                ObjectMetadata { created_at: Some(1), updated_at: None, project_id: None, source: None },
                ObjectPayload::Clipboard(ClipboardPayload {
                    text_content: text.into(),
                    detected_type: "text".into(),
                    pinned: false,
                    search_extra: None,
                }),
            ),
            Some(text.into()),
        )
    }

    #[test]
    fn ingest_stores_object_even_when_embedding_is_unavailable() {
        // No network/model in a test environment — embeddings::store is a
        // guaranteed no-op here, so this exercises exactly the "embedding
        // unavailable → pipeline continues" path.
        let conn = store_conn();
        let store = SqliteStore::new(&conn);
        let id = ingest(&store, file_ingestable("C:/p/main.rs")).unwrap();
        assert_eq!(id.object_type, ObjectType::File);

        let n: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "object must be saved regardless of embedding availability");
    }

    #[test]
    fn both_sources_share_the_same_ingest_call_with_no_branch() {
        // The point of this test: neither call site nor `ingest` itself
        // branches on which source it's handling.
        let conn = store_conn();
        let store = SqliteStore::new(&conn);

        let file_id = ingest(&store, file_ingestable("C:/p/a.rs")).unwrap();
        let clip_id = ingest(&store, clipboard_ingestable("hello")).unwrap();

        assert_eq!(file_id.object_type, ObjectType::File);
        assert_eq!(clip_id.object_type, ObjectType::Clipboard);
    }
}
