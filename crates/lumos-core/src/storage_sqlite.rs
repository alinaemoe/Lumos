//! SQLite implementation of the domain storage traits — infrastructure.
//!
//! This is the only module outside `db.rs` that is allowed to know table
//! names, column names, or SQL. It is also the only place that dispatches
//! on object type: `save`/`delete`/`resolve` each contain exactly one
//! `match`, and adding a source means adding an arm here rather than
//! touching the pipeline.
//!
//! `SqliteStore` borrows a `Connection` rather than owning one: the app
//! shares a single connection behind `Arc<Mutex<_>>` and the watchers each
//! open their own, and this phase deliberately doesn't disturb any of that.
//! Constructing one is free — it's a newtype over a reference.

use crate::object::{ObjectId, ObjectMetadata, ObjectPayload, ObjectType};
use crate::storage::{
    EmbeddingStore, NewObject, ObjectStore, ProjectPathEntry, ProjectStore,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// Object types this store can persist. Everything else errors rather than
/// returning empty — see `unsupported`.
const SUPPORTED: &[ObjectType] = &[ObjectType::File, ObjectType::Clipboard];

pub struct SqliteStore<'a> {
    conn: &'a Connection,
}

impl<'a> SqliteStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

/// The `object_type` discriminator as stored in the `embeddings` table.
/// These exact strings are already on disk in existing databases — they are
/// a storage format, not a display name, and must not be changed casually.
fn type_tag(t: ObjectType) -> Option<&'static str> {
    match t {
        ObjectType::File => Some("file"),
        ObjectType::Clipboard => Some("clipboard"),
        ObjectType::Screenshot
        | ObjectType::BrowserVisit
        | ObjectType::Session
        | ObjectType::Email => None,
    }
}

/// Source table and the column holding this type's embeddable text.
/// Both are compile-time constants chosen by an exhaustive match, so the
/// `format!` interpolations below cannot receive caller-controlled input.
fn source_table(t: ObjectType) -> Option<(&'static str, &'static str)> {
    match t {
        ObjectType::File => Some(("files", "filename")),
        ObjectType::Clipboard => Some(("clipboard_entries", "text_content")),
        ObjectType::Screenshot
        | ObjectType::BrowserVisit
        | ObjectType::Session
        | ObjectType::Email => None,
    }
}

/// A type with no table behind it. This is a wiring bug — the pipeline
/// produced an object for a source that was never given storage — so it is
/// reported loudly and uniformly.
///
/// Deliberately an error rather than a panic or `unreachable!`: these calls
/// happen on always-running background threads (the file watcher, the
/// clipboard poller, the backfill pass), and taking the process down over a
/// misconfigured source would be a worse failure than logging and
/// continuing. It is deliberately *not* an empty result, which was the
/// Phase 2 behaviour and made a half-wired source indistinguishable from
/// "no matches".
fn unsupported(t: ObjectType) -> anyhow::Error {
    anyhow::anyhow!(
        "object type {:?} has no storage backing (supported: {:?})",
        t,
        SUPPORTED
    )
}

fn vector_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_vector(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl SqliteStore<'_> {
    /// Files are keyed on their path: re-indexing a file that already
    /// exists must update it, not duplicate it.
    fn save_file(
        &self,
        meta: &ObjectMetadata,
        f: &crate::object::FilePayload,
    ) -> Result<ObjectId> {
        self.conn
            .execute(
                "INSERT INTO files (path, filename, extension, size, hash, project_id, created_at, modified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(path) DO UPDATE SET
                    filename = excluded.filename,
                    extension = excluded.extension,
                    size = excluded.size,
                    hash = excluded.hash,
                    project_id = excluded.project_id,
                    modified_at = excluded.modified_at",
                rusqlite::params![
                    f.path,
                    f.filename,
                    f.extension,
                    f.size,
                    f.hash,
                    meta.project_id,
                    meta.created_at,
                    meta.updated_at
                ],
            )
            .with_context(|| format!("upserting file {}", f.path))?;

        // `last_insert_rowid` is only correct for the INSERT branch — on
        // conflict-update it reports the previous insert, so the id has to
        // be read back explicitly.
        let id: i64 = self
            .conn
            .query_row("SELECT id FROM files WHERE path = ?1", [&f.path], |r| {
                r.get(0)
            })
            .context("reading back file id")?;
        Ok(ObjectId::new(ObjectType::File, id))
    }

    /// Clipboard entries are always new rows — the same text copied twice
    /// is two distinct events. Deduplication of *consecutive* repeats is a
    /// domain rule applied before save, not a storage constraint.
    fn save_clipboard(
        &self,
        meta: &ObjectMetadata,
        c: &crate::object::ClipboardPayload,
    ) -> Result<ObjectId> {
        self.conn
            .execute(
                "INSERT INTO clipboard_entries (content_type, text_content, detected_type, search_extra, source_app, pinned, created_at)
                 VALUES ('text', ?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    c.text_content,
                    c.detected_type,
                    c.search_extra,
                    meta.source,
                    c.pinned as i64,
                    meta.created_at
                ],
            )
            .context("inserting clipboard entry")?;
        Ok(ObjectId::new(
            ObjectType::Clipboard,
            self.conn.last_insert_rowid(),
        ))
    }

    /// Removes anything derived from an object. Called from `delete` so no
    /// caller has to remember it.
    fn remove_derived(&self, id: ObjectId) -> Result<()> {
        let Some(tag) = type_tag(id.object_type) else {
            return Ok(());
        };
        self.conn
            .execute(
                "DELETE FROM embeddings WHERE object_type = ?1 AND object_id = ?2",
                rusqlite::params![tag, id.local_id],
            )
            .context("deleting embedding")?;
        Ok(())
    }
}

impl ObjectStore for SqliteStore<'_> {
    fn supported_types(&self) -> &'static [ObjectType] {
        SUPPORTED
    }

    /// The single point of per-source dispatch in the whole system.
    fn save(&self, object: &NewObject) -> Result<ObjectId> {
        match &object.payload {
            ObjectPayload::File(f) => self.save_file(&object.metadata, f),
            ObjectPayload::Clipboard(c) => self.save_clipboard(&object.metadata, c),
            other => Err(unsupported(other.object_type())),
        }
    }

    fn delete(&self, id: ObjectId) -> Result<bool> {
        let table = match id.object_type {
            ObjectType::File => "files",
            ObjectType::Clipboard => "clipboard_entries",
            t => return Err(unsupported(t)),
        };
        // `table` is a compile-time constant from the match above.
        let n = self
            .conn
            .execute(&format!("DELETE FROM {table} WHERE id = ?1"), [id.local_id])
            .with_context(|| format!("deleting {:?} #{}", id.object_type, id.local_id))?;
        // Derived data dies with the object it came from — §10.2.
        self.remove_derived(id)?;
        Ok(n > 0)
    }

    fn resolve(&self, object_type: ObjectType, natural_key: &str) -> Result<Option<ObjectId>> {
        match object_type {
            ObjectType::File => {
                let id: Option<i64> = self
                    .conn
                    .query_row("SELECT id FROM files WHERE path = ?1", [natural_key], |r| {
                        r.get(0)
                    })
                    .optional()
                    .context("resolving file by path")?;
                Ok(id.map(|id| ObjectId::new(ObjectType::File, id)))
            }
            ObjectType::Clipboard => Err(anyhow::anyhow!(
                "clipboard entries have no natural key — they are identified by id only"
            )),
            t => Err(unsupported(t)),
        }
    }

    fn latest_text(&self, object_type: ObjectType) -> Result<Option<String>> {
        match object_type {
            ObjectType::Clipboard => self
                .conn
                .query_row(
                    "SELECT text_content FROM clipboard_entries
                     WHERE content_type = 'text'
                     ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map(Option::flatten)
                .context("reading latest clipboard entry"),
            ObjectType::File => Ok(None), // files aren't an event stream; nothing to dedupe
            t => Err(unsupported(t)),
        }
    }
}

impl ProjectStore for SqliteStore<'_> {
    fn get_or_create_project(&self, name: &str) -> Result<i64> {
        let existing: Option<i64> = self
            .conn
            .query_row("SELECT id FROM projects WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
            .context("looking up project by name")?;
        if let Some(id) = existing {
            return Ok(id);
        }

        let now = crate::now_secs();
        self.conn
            .execute(
                "INSERT INTO projects (name, created_at) VALUES (?1, ?2)",
                rusqlite::params![name, now],
            )
            .context("inserting project")?;
        Ok(self.conn.last_insert_rowid())
    }

    fn register_project_path(
        &self,
        project_id: i64,
        path: &str,
        repo_type: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO project_paths (project_id, path, repo_type)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET project_id = excluded.project_id, repo_type = excluded.repo_type",
                rusqlite::params![project_id, path, repo_type],
            )
            .context("registering project path")?;
        Ok(())
    }

    fn list_project_paths(&self) -> Result<Vec<ProjectPathEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT project_id, path FROM project_paths")?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectPathEntry {
                project_id: row.get(0)?,
                path: row.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

impl EmbeddingStore for SqliteStore<'_> {
    fn put_vector(&self, id: ObjectId, vector: &[f32]) -> Result<()> {
        let tag = type_tag(id.object_type).ok_or_else(|| unsupported(id.object_type))?;
        self.conn
            .execute(
                "INSERT INTO embeddings (object_type, object_id, vector) VALUES (?1, ?2, ?3)
                 ON CONFLICT(object_type, object_id) DO UPDATE SET vector = excluded.vector",
                rusqlite::params![tag, id.local_id, vector_to_bytes(vector)],
            )
            .context("storing embedding")?;
        Ok(())
    }

    /// Brute-force scan: load every vector of this type and score it
    /// against the query. Vectors are stored pre-normalised, so cosine
    /// similarity is a plain dot product.
    ///
    /// This is an implementation choice, not an interface constraint — at
    /// personal-history scale it costs milliseconds and needs no native
    /// extension. Replacing it with an ANN index changes this function
    /// only; no caller sees the difference.
    fn nearest(
        &self,
        object_type: ObjectType,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(i64, f32)>> {
        let tag = type_tag(object_type).ok_or_else(|| unsupported(object_type))?;
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = self
            .conn
            .prepare("SELECT object_id, vector FROM embeddings WHERE object_type = ?1")?;
        let rows = stmt.query_map([tag], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;

        let mut scored: Vec<(i64, f32)> = rows
            .filter_map(|r| r.ok())
            .map(|(id, bytes)| {
                let v = bytes_to_vector(&bytes);
                let score: f32 = query.iter().zip(v.iter()).map(|(a, b)| a * b).sum();
                (id, score)
            })
            .collect();

        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);
        Ok(scored)
    }

    fn pending_embeddings(
        &self,
        object_type: ObjectType,
        limit: usize,
    ) -> Result<Vec<(ObjectId, String)>> {
        let tag = type_tag(object_type).ok_or_else(|| unsupported(object_type))?;
        let (table, column) = source_table(object_type).ok_or_else(|| unsupported(object_type))?;
        // Both identifiers are compile-time constants from the match above.
        // SQLite treats a negative LIMIT as "no limit".
        let lim: i64 = if limit >= i64::MAX as usize {
            -1
        } else {
            limit as i64
        };
        let sql = format!(
            "SELECT t.id, t.{column} FROM {table} t
             LEFT JOIN embeddings e ON e.object_type = ?1 AND e.object_id = t.id
             WHERE e.object_id IS NULL AND t.{column} IS NOT NULL
             LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![tag, lim], |row| {
            Ok((
                ObjectId::new(object_type, row.get::<_, i64>(0)?),
                row.get::<_, String>(1)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{ClipboardPayload, FilePayload};

    fn store_conn() -> Connection {
        crate::db::open_in_memory().expect("in-memory schema")
    }

    fn file_object(path: &str) -> NewObject {
        NewObject::new(
            ObjectMetadata {
                created_at: Some(100),
                updated_at: Some(200),
                project_id: None,
                source: None,
            },
            ObjectPayload::File(FilePayload {
                path: path.into(),
                filename: "main.rs".into(),
                extension: Some("rs".into()),
                size: 12,
                hash: Some("deadbeef".into()),
                project_name: None,
            }),
        )
    }

    fn clip_object(text: &str) -> NewObject {
        NewObject::new(
            ObjectMetadata {
                created_at: Some(500),
                updated_at: None,
                project_id: None,
                source: Some("VS Code".into()),
            },
            ObjectPayload::Clipboard(ClipboardPayload {
                text_content: text.into(),
                detected_type: "sql".into(),
                pinned: false,
                search_extra: Some("sql query database".into()),
            }),
        )
    }

    #[test]
    fn saving_a_file_twice_updates_rather_than_duplicates() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let a = st.save(&file_object("C:/p/main.rs")).unwrap();
        let b = st.save(&file_object("C:/p/main.rs")).unwrap();
        assert_eq!(a, b, "same path must resolve to the same identity");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn saving_clipboard_twice_creates_two_events() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let a = st.save(&clip_object("SELECT 1")).unwrap();
        let b = st.save(&clip_object("SELECT 1")).unwrap();
        assert_ne!(a, b, "clipboard copies are distinct events");
    }

    #[test]
    fn delete_removes_the_object_and_its_embedding() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&file_object("C:/p/main.rs")).unwrap();
        st.put_vector(id, &[0.1, 0.2, 0.3]).unwrap();

        assert!(st.delete(id).unwrap(), "row existed");

        let files: i64 = conn
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        let vecs: i64 = conn
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(files, 0);
        assert_eq!(vecs, 0, "derived data must not outlive its object (§10.2)");
        assert!(!st.delete(id).unwrap(), "second delete is a no-op");
    }

    #[test]
    fn resolve_maps_natural_key_to_identity() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&file_object("C:/p/main.rs")).unwrap();
        assert_eq!(
            st.resolve(ObjectType::File, "C:/p/main.rs").unwrap(),
            Some(id)
        );
        assert_eq!(st.resolve(ObjectType::File, "C:/p/gone.rs").unwrap(), None);
    }

    #[test]
    fn unsupported_types_error_rather_than_returning_empty() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let ghost = ObjectId::new(ObjectType::Screenshot, 1);

        assert!(st.delete(ghost).is_err());
        assert!(st.resolve(ObjectType::Screenshot, "x").is_err());
        assert!(st.latest_text(ObjectType::Email).is_err());
        assert!(st.put_vector(ghost, &[0.1]).is_err());
        assert!(st.nearest(ObjectType::Screenshot, &[0.1], 5).is_err());
        assert!(st.pending_embeddings(ObjectType::BrowserVisit, 10).is_err());
        assert!(!st.supported_types().contains(&ObjectType::Screenshot));
    }

    #[test]
    fn pending_embeddings_returns_text_with_ids() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&clip_object("SELECT 1")).unwrap();

        let pending = st.pending_embeddings(ObjectType::Clipboard, usize::MAX).unwrap();
        assert_eq!(pending, vec![(id, "SELECT 1".to_string())]);

        st.put_vector(id, &[0.5; 4]).unwrap();
        assert!(st
            .pending_embeddings(ObjectType::Clipboard, usize::MAX)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn nearest_ranks_by_similarity() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let a = st.save(&clip_object("a")).unwrap();
        let b = st.save(&clip_object("b")).unwrap();
        st.put_vector(a, &[1.0, 0.0]).unwrap();
        st.put_vector(b, &[0.0, 1.0]).unwrap();

        let hits = st.nearest(ObjectType::Clipboard, &[0.9, 0.1], 2).unwrap();
        assert_eq!(hits[0].0, a.local_id, "closest vector must rank first");
        assert_eq!(hits.len(), 2);

        let capped = st.nearest(ObjectType::Clipboard, &[0.9, 0.1], 1).unwrap();
        assert_eq!(capped.len(), 1, "limit is honoured");
    }
}
