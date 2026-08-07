//! Domain-owned storage interfaces — see `ARCHITECTURE_INVARIANTS.md` §6.3.
//!
//! These traits describe *what Lumos needs to persist*, not how SQLite does
//! it. No `rusqlite` types, no SQL, no table or column names appear here or
//! in any signature below. The SQLite implementation lives in
//! `storage_sqlite.rs` and is the only module allowed to know those things.
//!
//! Deliberately narrow. Each trait covers one capability so a caller can
//! declare exactly what it needs (`fn f<S: FileStore + ProjectStore>`)
//! rather than depending on a god-object `Storage`. Operations are added
//! when a real caller needs them — there are no speculative methods here.
//!
//! **What is *not* behind these traits, on purpose:** decisions. Storage
//! hands back data; the domain decides what it means. `list_project_paths`
//! returns every registered path and the domain picks the longest matching
//! prefix; `load_vectors` returns vectors and the domain scores them. That
//! split is what keeps business rules testable without a database.

use crate::object::{ObjectId, ObjectType};
use anyhow::Result;

/// Everything the domain knows about a file at the moment it wants it
/// persisted. Mirrors the ingestion side only — reads go through
/// `search`/query paths, which are still SQLite-coupled (Phase 4).
#[derive(Debug, Clone)]
pub struct FileUpsert {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size: i64,
    pub hash: Option<String>,
    pub project_id: Option<i64>,
    pub created_at: i64,
    pub modified_at: i64,
}

/// A registered project folder. Returned as plain data so the caller can
/// run its own matching logic — see the module note on decisions.
#[derive(Debug, Clone)]
pub struct ProjectPathEntry {
    pub project_id: i64,
    pub path: String,
}

/// A clipboard entry ready to persist: already classified, already
/// truncated, already deduplicated by the domain.
#[derive(Debug, Clone)]
pub struct ClipboardInsert {
    pub text: String,
    pub detected_type: String,
    pub search_extra: String,
    pub source_app: Option<String>,
    pub created_at: i64,
}

pub trait ProjectStore {
    fn get_or_create_project(&self, name: &str) -> Result<i64>;
    fn register_project_path(
        &self,
        project_id: i64,
        path: &str,
        repo_type: Option<&str>,
    ) -> Result<()>;
    /// Every registered path. The caller decides which one wins.
    fn list_project_paths(&self) -> Result<Vec<ProjectPathEntry>>;
}

pub trait FileStore {
    /// Insert or update by path; returns the stored id either way.
    fn upsert_file(&self, file: &FileUpsert) -> Result<i64>;
    /// Removes the file and returns the id it had, if it existed — the
    /// caller needs that id to clean up anything derived from it
    /// (`ARCHITECTURE_INVARIANTS.md` §10.2).
    fn remove_file_by_path(&self, path: &str) -> Result<Option<i64>>;
}

pub trait ClipboardStore {
    /// Text of the most recent entry, for the domain's duplicate check.
    fn latest_text(&self) -> Result<Option<String>>;
    fn insert_entry(&self, entry: &ClipboardInsert) -> Result<i64>;
}

pub trait EmbeddingStore {
    fn put_vector(&self, id: ObjectId, vector: &[f32]) -> Result<()>;
    fn remove_vector(&self, id: ObjectId) -> Result<()>;
    /// Every vector of a given type, decoded. Decoding is storage's job —
    /// the domain should never see a byte buffer it has to interpret.
    ///
    /// This loads the full set into memory by design at current data
    /// volumes; the brute-force scan that consumes it is a known and
    /// documented scaling limit (see `embeddings.rs`). The trait is shaped
    /// so replacing it with an ANN index later changes this one method,
    /// not its callers.
    fn load_vectors(&self, object_type: ObjectType) -> Result<Vec<(i64, Vec<f32>)>>;
    /// Ids of that type with no vector yet — the backfill work list.
    fn ids_missing_vectors(&self, object_type: ObjectType) -> Result<Vec<i64>>;
    /// The text that represents this object for embedding purposes.
    /// Which field that is (a file's name, a clipboard entry's content) is
    /// a storage detail the domain doesn't need to know.
    fn embeddable_text(&self, id: ObjectId) -> Result<Option<String>>;
}
