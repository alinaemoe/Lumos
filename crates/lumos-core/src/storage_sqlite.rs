//! SQLite implementation of the domain storage traits — infrastructure.
//!
//! This is the only module outside `db.rs` that is allowed to know table
//! names, column names, or SQL. Everything above it speaks in domain types.
//!
//! `SqliteStore` borrows a `Connection` rather than owning one: the app
//! shares a single connection behind `Arc<Mutex<_>>` and the watchers each
//! open their own, and this phase deliberately doesn't disturb any of that.
//! Constructing one is free — it's a newtype over a reference.

use crate::object::{ObjectId, ObjectType};
use crate::storage::{
    ClipboardInsert, ClipboardStore, EmbeddingStore, FileStore, FileUpsert, ProjectPathEntry,
    ProjectStore,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

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
///
/// `None` means "this object type has no storage yet" — Screenshot,
/// BrowserVisit, Session and Email are declared in the domain (Phase 1) but
/// have no tables and no ingestion. Reads for them return empty, writes
/// error, and neither is a silent no-op.
fn type_tag(t: ObjectType) -> Option<&'static str> {
    match t {
        ObjectType::File => Some("file"),
        ObjectType::Clipboard => Some("clipboard"),
        ObjectType::Screenshot | ObjectType::BrowserVisit | ObjectType::Session | ObjectType::Email => None,
    }
}

/// Source table and the column holding this type's embeddable text.
/// Both are compile-time constants chosen by an exhaustive match, so the
/// `format!` interpolation below cannot receive caller-controlled input —
/// this replaces the previous `table: &str` parameter that a caller could
/// in principle have passed anything to.
fn source_table(t: ObjectType) -> Option<(&'static str, &'static str)> {
    match t {
        ObjectType::File => Some(("files", "filename")),
        ObjectType::Clipboard => Some(("clipboard_entries", "text_content")),
        ObjectType::Screenshot | ObjectType::BrowserVisit | ObjectType::Session | ObjectType::Email => None,
    }
}

fn unsupported(t: ObjectType) -> anyhow::Error {
    anyhow::anyhow!("object type {:?} has no storage backing yet", t)
}

fn vector_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_vector(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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

impl FileStore for SqliteStore<'_> {
    fn upsert_file(&self, f: &FileUpsert) -> Result<i64> {
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
                    f.project_id,
                    f.created_at,
                    f.modified_at
                ],
            )
            .with_context(|| format!("upserting file {}", f.path))?;

        // `last_insert_rowid` is only correct for the INSERT branch — on
        // conflict-update it reports the previous insert, so the id has to
        // be read back explicitly. Same query the pre-boundary code ran.
        self.conn
            .query_row("SELECT id FROM files WHERE path = ?1", [&f.path], |r| {
                r.get(0)
            })
            .context("reading back file id")
    }

    fn remove_file_by_path(&self, path: &str) -> Result<Option<i64>> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM files WHERE path = ?1", [path], |r| r.get(0))
            .optional()
            .context("looking up file id before delete")?;
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", [path])
            .context("deleting file row")?;
        Ok(id)
    }
}

impl ClipboardStore for SqliteStore<'_> {
    fn latest_text(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT text_content FROM clipboard_entries
                 WHERE content_type = 'text'
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .context("reading latest clipboard entry")
    }

    fn insert_entry(&self, e: &ClipboardInsert) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO clipboard_entries (content_type, text_content, detected_type, search_extra, source_app, created_at)
                 VALUES ('text', ?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![e.text, e.detected_type, e.search_extra, e.source_app, e.created_at],
            )
            .context("inserting clipboard entry")?;
        Ok(self.conn.last_insert_rowid())
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

    fn remove_vector(&self, id: ObjectId) -> Result<()> {
        let Some(tag) = type_tag(id.object_type) else {
            return Ok(()); // nothing could have been stored for it
        };
        self.conn
            .execute(
                "DELETE FROM embeddings WHERE object_type = ?1 AND object_id = ?2",
                rusqlite::params![tag, id.local_id],
            )
            .context("deleting embedding")?;
        Ok(())
    }

    fn load_vectors(&self, object_type: ObjectType) -> Result<Vec<(i64, Vec<f32>)>> {
        let Some(tag) = type_tag(object_type) else {
            return Ok(Vec::new());
        };
        let mut stmt = self
            .conn
            .prepare("SELECT object_id, vector FROM embeddings WHERE object_type = ?1")?;
        let rows = stmt.query_map([tag], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        Ok(rows
            .filter_map(|r| r.ok())
            .map(|(id, bytes)| (id, bytes_to_vector(&bytes)))
            .collect())
    }

    fn ids_missing_vectors(&self, object_type: ObjectType) -> Result<Vec<i64>> {
        let (Some(tag), Some((table, _))) = (type_tag(object_type), source_table(object_type))
        else {
            return Ok(Vec::new());
        };
        // `table` is a compile-time constant from the match above.
        let sql = format!(
            "SELECT t.id FROM {table} t
             LEFT JOIN embeddings e ON e.object_type = ?1 AND e.object_id = t.id
             WHERE e.object_id IS NULL"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([tag], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn embeddable_text(&self, id: ObjectId) -> Result<Option<String>> {
        let Some((table, column)) = source_table(id.object_type) else {
            return Ok(None);
        };
        // Both identifiers are compile-time constants from the match above.
        let sql = format!("SELECT {column} FROM {table} WHERE id = ?1");
        self.conn
            .query_row(&sql, [id.local_id], |r| r.get::<_, Option<String>>(0))
            .optional()
            .map(Option::flatten)
            .context("reading embeddable text")
    }
}
