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

use crate::object::{
    ClipboardPayload, FilePayload, Object, ObjectId, ObjectMetadata, ObjectPayload, ObjectType,
};
use crate::storage::{
    EmbeddingStore, FullTextIndex, NewObject, ObjectStore, ProjectPathEntry, ProjectStore,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;

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

/// FTS5 syntax escaping. A near-duplicate of `search::sanitize_fts_query`,
/// kept local on purpose: this is an FTS5-syntax detail (which characters
/// need escaping in a `MATCH` expression), not domain logic, and having
/// `storage_sqlite.rs` — the infrastructure layer — reach *up* into
/// `search.rs` to borrow it would invert the one dependency direction this
/// module exists to enforce. Five lines of duplication is cheaper than that
/// inversion. If this drifts out of sync with `search.rs`'s copy, that's a
/// sign both should move to a shared low-level (non-domain) text-escaping
/// module — not a reason to import across the boundary today.
fn sanitize_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
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

    /// Batched load of files by id — one query for the whole set, not one
    /// per id. Selects every column `ObjectMetadata`/`FilePayload` can
    /// carry, including `created_at`/`modified_at`/`project_id`, which
    /// `search::SearchResult` (a separate, older read path) does not
    /// project. This is what makes those fields reachable for future
    /// ranking without touching the schema.
    fn load_files(&self, ids: &[i64]) -> Result<Vec<Object>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
        // `placeholders` is built from `ids.len()` only, never from
        // caller-supplied text — no injection surface.
        let sql = format!(
            "SELECT f.id, f.path, f.filename, f.extension, f.size, f.hash,
                    f.project_id, f.created_at, f.modified_at, p.name
             FROM files f LEFT JOIN projects p ON p.id = f.project_id
             WHERE f.id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok(Object::new(
                row.get::<_, i64>(0)?,
                ObjectMetadata {
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                    project_id: row.get(6)?,
                    source: None,
                },
                ObjectPayload::File(FilePayload {
                    path: row.get(1)?,
                    filename: row.get(2)?,
                    extension: row.get(3)?,
                    size: row.get(4)?,
                    hash: row.get(5)?,
                    project_name: row.get(9)?,
                }),
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Batched load of clipboard entries by id — same reasoning as
    /// `load_files`.
    fn load_clipboard(&self, ids: &[i64]) -> Result<Vec<Object>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, text_content, detected_type, search_extra, source_app, pinned, created_at
             FROM clipboard_entries WHERE id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok(Object::new(
                row.get::<_, i64>(0)?,
                ObjectMetadata {
                    created_at: row.get(6)?,
                    updated_at: None,
                    project_id: None,
                    source: row.get(4)?,
                },
                ObjectPayload::Clipboard(ClipboardPayload {
                    text_content: row.get(1)?,
                    detected_type: row.get(2)?,
                    pinned: row.get::<_, i64>(5)? != 0,
                    search_extra: row.get(3)?,
                }),
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
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

    /// File recency is `modified_at`; clipboard recency is insertion order
    /// (`id DESC`), not `created_at DESC` — these are not the same thing
    /// (a clock change could desync them) but `id DESC` is what
    /// `clipboard::search`'s pre-Phase-4 no-text branch always used, and
    /// this method exists specifically to preserve that behavior exactly,
    /// not to improve on it.
    fn recent(
        &self,
        object_type: ObjectType,
        since: i64,
        until: Option<i64>,
        detected_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ObjectId>> {
        let (table, order_by, since_column) = match object_type {
            ObjectType::File => ("files", "modified_at DESC", "modified_at"),
            ObjectType::Clipboard => ("clipboard_entries", "id DESC", "created_at"),
            t => return Err(unsupported(t)),
        };
        // `files` has no `detected_type` column and no such concept —
        // silently ignored for it, same "no opinion" rule
        // `matches_detected_type` already applies at the domain layer,
        // rather than treating a meaningless constraint as an error.
        let detected_type = detected_type.filter(|_| object_type == ObjectType::Clipboard);

        // Both `until` and `detected_type` are folded into the WHERE
        // clause itself, before LIMIT — that ordering is the entire fix,
        // identical in shape to the earlier `until` fix and for the same
        // reason: applying either only after LIMIT already ran is what
        // let unrelated newer rows crowd the actual matches out of the
        // result before the filter ever got a chance to run.
        // `table`/`order_by`/`since_column` are compile-time constants
        // from the match above — never caller-controlled — so building
        // SQL with `format!` here carries no injection risk. The
        // detected_type *value* is always bound as a parameter, never
        // interpolated into the SQL text.
        let mut sql = format!("SELECT id FROM {table} WHERE {since_column} >= ?1");
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(since)];
        if let Some(u) = until {
            sql.push_str(&format!(" AND {since_column} <= ?{}", params.len() + 1));
            params.push(Box::new(u));
        }
        if let Some(dt) = detected_type {
            sql.push_str(&format!(" AND detected_type = ?{}", params.len() + 1));
            params.push(Box::new(dt.to_string()));
        }
        sql.push_str(&format!(" ORDER BY {order_by} LIMIT ?{}", params.len() + 1));
        params.push(Box::new(limit as i64));
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(ObjectId::new(object_type, row.get::<_, i64>(0)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn load(&self, ids: &[ObjectId]) -> Result<Vec<Object>> {
        let mut by_type: HashMap<ObjectType, Vec<i64>> = HashMap::new();
        for id in ids {
            by_type.entry(id.object_type).or_default().push(id.local_id);
        }

        let mut out = Vec::with_capacity(ids.len());
        for (object_type, local_ids) in by_type {
            match object_type {
                ObjectType::File => out.extend(self.load_files(&local_ids)?),
                ObjectType::Clipboard => out.extend(self.load_clipboard(&local_ids)?),
                t => return Err(unsupported(t)),
            }
        }
        Ok(out)
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

impl FullTextIndex for SqliteStore<'_> {
    /// Queries the FTS5 virtual table directly (`SELECT rowid, rank FROM
    /// <fts_table> WHERE <fts_table> MATCH ?`) rather than joining back to
    /// `files`/`clipboard_entries` — this trait returns bare ids and
    /// scores, the same shape as `EmbeddingStore::nearest`, so there is
    /// nothing else to select. Hydrating those ids into full objects is
    /// `ObjectStore::load`'s job, not this one's.
    ///
    /// FTS5's own `rank` is smaller-is-better; negated here so the return
    /// value matches `nearest`'s higher-is-better convention (see the
    /// trait doc — this is a sign-convention alignment only, *not* a scale
    /// alignment).
    fn search_text(&self, object_type: ObjectType, query: &str, limit: usize) -> Result<Vec<(i64, f32)>> {
        let fts_table = match object_type {
            ObjectType::File => "files_fts",
            ObjectType::Clipboard => "clipboard_fts",
            t => return Err(unsupported(t)),
        };
        let fts_query = sanitize_fts_query(query);
        if fts_query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // `fts_table` is a compile-time constant from the match above.
        let sql = format!(
            "SELECT rowid, rank FROM {fts_table} WHERE {fts_table} MATCH ?1 ORDER BY rank LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })?;
        rows.map(|r| r.map(|(id, rank)| (id, -(rank as f32))).map_err(anyhow::Error::from))
            .collect()
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

    #[test]
    fn load_returns_full_file_metadata_not_projected_by_search_rs() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let project_id = st.get_or_create_project("demo").unwrap();
        let mut object = file_object("C:/p/main.rs");
        object.metadata.project_id = Some(project_id);
        let id = st.save(&object).unwrap();

        let loaded = st.load(&[id]).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].metadata.created_at, Some(100));
        assert_eq!(loaded[0].metadata.updated_at, Some(200));
        assert_eq!(
            loaded[0].metadata.project_id,
            Some(project_id),
            "created_at/updated_at/project_id must be reachable via load(), unlike search::SearchResult"
        );
    }

    #[test]
    fn load_returns_clipboard_object() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&clip_object("SELECT 1")).unwrap();

        let loaded = st.load(&[id]).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, id);
        match &loaded[0].payload {
            ObjectPayload::Clipboard(c) => assert_eq!(c.text_content, "SELECT 1"),
            _ => panic!("expected Clipboard payload"),
        }
    }

    #[test]
    fn load_batches_mixed_types_in_one_call_and_skips_missing_ids() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let file_id = st.save(&file_object("C:/p/main.rs")).unwrap();
        let clip_id = st.save(&clip_object("SELECT 1")).unwrap();
        let ghost = ObjectId::new(ObjectType::File, 999_999);

        let loaded = st.load(&[file_id, clip_id, ghost]).unwrap();
        assert_eq!(loaded.len(), 2, "a missing id is omitted, not an error");
        let ids: std::collections::HashSet<_> = loaded.iter().map(|o| o.id).collect();
        assert!(ids.contains(&file_id));
        assert!(ids.contains(&clip_id));
    }

    #[test]
    fn load_of_empty_slice_is_empty_and_unsupported_type_errors() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        assert!(st.load(&[]).unwrap().is_empty());
        assert!(st.load(&[ObjectId::new(ObjectType::Screenshot, 1)]).is_err());
    }

    #[test]
    fn search_text_finds_file_and_clipboard_candidates() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let file_id = st.save(&file_object("C:/p/hydration_notes.rs")).unwrap();
        let clip_id = st.save(&clip_object("hydration failed in react")).unwrap();

        let file_hits = st.search_text(ObjectType::File, "hydration_notes", 10).unwrap();
        assert_eq!(file_hits.len(), 1);
        assert_eq!(file_hits[0].0, file_id.local_id);

        let clip_hits = st.search_text(ObjectType::Clipboard, "hydration", 10).unwrap();
        assert_eq!(clip_hits.len(), 1);
        assert_eq!(clip_hits[0].0, clip_id.local_id);
    }

    #[test]
    fn search_text_ranks_more_relevant_match_first() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        // "react" appears once in b, three times in a — a should rank higher.
        let a = st.save(&clip_object("react react react error")).unwrap();
        let b = st.save(&clip_object("a single react mention")).unwrap();

        let hits = st.search_text(ObjectType::Clipboard, "react", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, a.local_id, "more relevant match must rank first");
        assert_eq!(hits[1].0, b.local_id);
        assert!(hits[0].1 > hits[1].1, "higher score must mean more relevant, matching nearest()'s convention");
    }

    #[test]
    fn search_text_empty_query_and_unsupported_type() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        assert!(st.search_text(ObjectType::File, "   ", 10).unwrap().is_empty());
        assert!(st.search_text(ObjectType::Screenshot, "x", 10).is_err());
    }

    fn file_object_at(path: &str, modified_at: i64) -> NewObject {
        let mut obj = file_object(path);
        obj.metadata.updated_at = Some(modified_at);
        obj
    }

    #[test]
    fn recent_orders_files_by_modified_at_desc() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let old = st.save(&file_object_at("C:/p/old.rs", 100)).unwrap();
        let new = st.save(&file_object_at("C:/p/new.rs", 300)).unwrap();
        let mid = st.save(&file_object_at("C:/p/mid.rs", 200)).unwrap();

        let ids = st.recent(ObjectType::File, 0, None, None, 10).unwrap();
        assert_eq!(ids, vec![new, mid, old], "most-recently-modified first");
    }

    #[test]
    fn recent_respects_since_and_limit() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&file_object_at("C:/p/old.rs", 100)).unwrap();
        let new = st.save(&file_object_at("C:/p/new.rs", 300)).unwrap();

        let ids = st.recent(ObjectType::File, 200, None, None, 10).unwrap();
        assert_eq!(ids, vec![new], "rows before `since` must be excluded");

        let capped = st.recent(ObjectType::File, 0, None, None, 1).unwrap();
        assert_eq!(capped.len(), 1, "limit is honoured");
    }

    #[test]
    fn recent_orders_clipboard_by_insertion_not_declared_timestamp() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        // Inserted in this order — `recent` must reproduce id-DESC order,
        // matching pre-Phase-4 `clipboard::search`'s no-text branch exactly,
        // not a re-sort by `created_at`.
        let first = st.save(&clip_object("first")).unwrap();
        let second = st.save(&clip_object("second")).unwrap();

        let ids = st.recent(ObjectType::Clipboard, 0, None, None, 10).unwrap();
        assert_eq!(ids, vec![second, first]);
    }

    #[test]
    fn recent_unsupported_type_errors() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        assert!(st.recent(ObjectType::Screenshot, 0, None, None, 10).is_err());
    }

    // ---- Regression: recent()'s `until` must be enforced before LIMIT,
    // not after (the bug: enough newer rows could exhaust the fetch budget
    // before any in-window row was ever retrieved). ----

    #[test]
    fn recent_with_until_finds_in_window_clipboard_rows_when_few_newer_rows_exist() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let since = 1_000;
        let until = 2_000;

        let in_window: Vec<ObjectId> = (0..3)
            .map(|i| st.save(&clip_object(&format!("yesterday {i}"))).unwrap())
            .collect();
        // A handful of newer rows — not enough to challenge a naive fix,
        // this is the easy case named explicitly in the requirements.
        for i in 0..5 {
            st.save(&clip_object(&format!("today {i}"))).unwrap();
        }
        // Backdate the "in_window" rows into [since, until]; leave the
        // "today" rows at their real (newer) insertion-time created_at.
        for id in &in_window {
            conn.execute(
                "UPDATE clipboard_entries SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![since + 100, id.local_id],
            )
            .unwrap();
        }

        let ids = st.recent(ObjectType::Clipboard, since, Some(until), None, 60).unwrap();
        for id in &in_window {
            assert!(ids.contains(id), "in-window row {id:?} missing with only a few newer rows present");
        }
    }

    #[test]
    fn recent_with_until_finds_in_window_clipboard_rows_even_when_many_more_newer_rows_exist() {
        // The actual bug reproduction: 70 "today" rows, 3 "yesterday" rows,
        // fetch_n = 60. Pre-fix, `until` was applied only after this call
        // returned, so the 70 newer rows (inserted with higher ids, so
        // `ORDER BY id DESC LIMIT 60` picked them first) filled the entire
        // budget and all 3 in-window rows vanished with zero returned.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let since = 1_000_000;
        let until = 2_000_000;

        // In-window rows inserted FIRST so they get the lowest ids — the
        // realistic case, since older events are always inserted earlier
        // than newer ones. This is exactly what defeated the pre-fix
        // `ORDER BY id DESC LIMIT` when `until` wasn't in the WHERE clause.
        let in_window: Vec<ObjectId> = (0..3)
            .map(|i| st.save(&clip_object(&format!("yesterday joke {i}"))).unwrap())
            .collect();
        for id in &in_window {
            conn.execute(
                "UPDATE clipboard_entries SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![since + 100, id.local_id],
            )
            .unwrap();
        }
        for i in 0..70 {
            let id = st.save(&clip_object(&format!("today filler {i}"))).unwrap();
            conn.execute(
                "UPDATE clipboard_entries SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![until + 10_000 + i, id.local_id],
            )
            .unwrap();
        }

        let ids = st.recent(ObjectType::Clipboard, since, Some(until), None, 60).unwrap();
        assert_eq!(
            ids.len(),
            3,
            "all 3 in-window rows must survive regardless of how many newer rows exist"
        );
        for id in &in_window {
            assert!(ids.contains(id), "in-window row {id:?} was crowded out by newer rows — the exact bug");
        }
    }

    #[test]
    fn recent_with_until_excludes_rows_newer_than_until() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let since = 1_000;
        let until = 2_000;

        let in_window = st.save(&clip_object("in window")).unwrap();
        conn.execute(
            "UPDATE clipboard_entries SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![since + 50, in_window.local_id],
        )
        .unwrap();
        let too_new = st.save(&clip_object("too new")).unwrap();
        conn.execute(
            "UPDATE clipboard_entries SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![until + 50, too_new.local_id],
        )
        .unwrap();

        let ids = st.recent(ObjectType::Clipboard, since, Some(until), None, 60).unwrap();
        assert_eq!(ids, vec![in_window], "a row newer than `until` must not be returned");
    }

    #[test]
    fn recent_without_until_behaves_exactly_as_before() {
        // `today`/`week`-style calls (`until: None`) must be completely
        // unaffected by this fix.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let old = st.save(&file_object_at("C:/p/old.rs", 100)).unwrap();
        let new = st.save(&file_object_at("C:/p/new.rs", 300)).unwrap();

        let ids = st.recent(ObjectType::File, 0, None, None, 10).unwrap();
        assert_eq!(ids, vec![new, old], "since-only ordering/inclusion unchanged");
    }

    #[test]
    fn recent_with_until_works_for_file_type_too() {
        // Point 5 of the requirements: both supported object types, not
        // just Clipboard.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let since = 1_000_000;
        let until = 2_000_000;

        let in_window = st.save(&file_object_at("C:/p/yesterday.rs", since + 500)).unwrap();
        // Many newer files, same crowding shape as the clipboard case.
        for i in 0..70 {
            st.save(&file_object_at(&format!("C:/p/today_{i}.rs"), until + 10_000 + i))
                .unwrap();
        }

        let ids = st.recent(ObjectType::File, since, Some(until), None, 60).unwrap();
        assert_eq!(ids, vec![in_window], "the one in-window file must survive 70 newer files");
    }

    // ---- Regression: recent()'s `detected_type` must be enforced before
    // LIMIT, not after (the `type:jwt` bug: enough newer non-matching rows
    // could exhaust the fetch budget before any matching row was ever
    // fetched — reproduced live on real data). ----

    #[test]
    fn recent_with_detected_type_finds_matching_rows_when_few_unrelated_rows_exist() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);

        let jwt_rows: Vec<ObjectId> = (0..3)
            .map(|i| {
                st.save(&NewObject::new(
                    ObjectMetadata { created_at: Some(0), ..ObjectMetadata::default() },
                    ObjectPayload::Clipboard(ClipboardPayload {
                        text_content: format!("jwt token {i}"),
                        detected_type: "jwt".into(),
                        pinned: false,
                        search_extra: None,
                    }),
                ))
                .unwrap()
            })
            .collect();
        for i in 0..5 {
            st.save(&clip_object(&format!("unrelated {i}"))).unwrap();
        }

        let ids = st.recent(ObjectType::Clipboard, 0, None, Some("jwt"), 60).unwrap();
        for id in &jwt_rows {
            assert!(ids.contains(id), "jwt row {id:?} missing with only a few unrelated rows present");
        }
        assert_eq!(ids.len(), 3, "no non-jwt row must be included");
    }

    #[test]
    fn recent_with_detected_type_finds_matching_rows_even_when_many_more_unrelated_rows_exist() {
        // The actual reported bug, reproduced exactly: 3 real `jwt` rows,
        // 70 unrelated newer rows, fetch_n=60. Pre-fix, `detected_type`
        // was applied only after this call returned, so — with jwt rows
        // inserted first, giving them the lowest ids, exactly as happens
        // in real use — `ORDER BY id DESC LIMIT 60` picked the 70 newer
        // unrelated rows and the 3 real jwt rows never entered the
        // candidate set at all. This is precisely what was observed on
        // the real live database: unrelated clipboard growth pushed the
        // 60-most-recent-id cutoff past all three real `jwt` rows.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);

        let jwt_rows: Vec<ObjectId> = (0..3)
            .map(|i| {
                st.save(&NewObject::new(
                    ObjectMetadata { created_at: Some(0), ..ObjectMetadata::default() },
                    ObjectPayload::Clipboard(ClipboardPayload {
                        text_content: format!("jwt token {i}"),
                        detected_type: "jwt".into(),
                        pinned: false,
                        search_extra: None,
                    }),
                ))
                .unwrap()
            })
            .collect();
        for i in 0..70 {
            st.save(&clip_object(&format!("unrelated filler {i}"))).unwrap();
        }

        let ids = st.recent(ObjectType::Clipboard, 0, None, Some("jwt"), 60).unwrap();
        assert_eq!(
            ids.len(),
            3,
            "all 3 jwt rows must survive regardless of how many unrelated newer rows exist"
        );
        for id in &jwt_rows {
            assert!(ids.contains(id), "jwt row {id:?} was crowded out by unrelated rows — the exact bug");
        }
    }

    #[test]
    fn recent_with_detected_type_excludes_non_matching_rows() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);

        let jwt = st
            .save(&NewObject::new(
                ObjectMetadata { created_at: Some(0), ..ObjectMetadata::default() },
                ObjectPayload::Clipboard(ClipboardPayload {
                    text_content: "jwt token".into(),
                    detected_type: "jwt".into(),
                    pinned: false,
                    search_extra: None,
                }),
            ))
            .unwrap();
        st.save(&clip_object("plain text, not jwt")).unwrap();

        let ids = st.recent(ObjectType::Clipboard, 0, None, Some("jwt"), 60).unwrap();
        assert_eq!(ids, vec![jwt], "a non-matching row must not be returned");
    }

    #[test]
    fn recent_without_detected_type_behaves_exactly_as_before() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let first = st.save(&clip_object("first")).unwrap();
        let second = st.save(&clip_object("second")).unwrap();

        let ids = st.recent(ObjectType::Clipboard, 0, None, None, 10).unwrap();
        assert_eq!(ids, vec![second, first], "no detected_type filter must be a no-op, identical to before this fix");
    }

    #[test]
    fn recent_with_detected_type_is_ignored_for_file_rather_than_erroring() {
        // Files have no detected_type column and no such concept. This
        // must not error and must not silently return nothing — it must
        // behave exactly as if `detected_type` were `None`, mirroring
        // `matches_detected_type`'s "no opinion" rule at the domain layer.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let file = st.save(&file_object_at("C:/p/a.rs", 100)).unwrap();

        let ids = st.recent(ObjectType::File, 0, None, Some("jwt"), 10).unwrap();
        assert_eq!(ids, vec![file], "detected_type must be a no-op for File, not an error or an empty result");
    }
}
