//! Domain-owned storage interfaces — see `ARCHITECTURE_INVARIANTS.md` §6.3.
//!
//! These traits describe *what Lumos needs to persist*, not how SQLite does
//! it. No `rusqlite` types, no SQL, no table or column names appear here or
//! in any signature below. The SQLite implementation lives in
//! `storage_sqlite.rs` and is the only module allowed to know those things.
//!
//! # Object-oriented, not source-oriented
//!
//! The boundary speaks `Object` (Phase 1's domain language), not `File` or
//! `Clipboard`. There is one `save`, one `delete`, one `resolve` — the
//! per-source dispatch lives *inside* the implementation. This is what lets
//! Phase 3 write a single `Store` stage instead of a `match` over every
//! source, and what makes adding Screenshots/Browser/Email a change to
//! `storage_sqlite.rs` rather than to the pipeline.
//!
//! Phase 2 got this wrong: it exposed `FileStore::upsert_file(&FileUpsert)`
//! and `ClipboardStore::insert_entry(&ClipboardInsert)`, which forced every
//! caller to know which source it was dealing with. Phase 2.1 replaces
//! those with the object-level API below.
//!
//! # Where decisions live
//!
//! Storage answers questions; the domain decides what the answers mean.
//! `list_project_paths` returns every registered path and the domain picks
//! the longest matching prefix. `latest_text` returns text and the domain
//! decides whether it counts as a duplicate.
//!
//! The one deliberate exception is [`EmbeddingStore::nearest`]: nearest-
//! neighbour lookup is an *index* operation, the vector equivalent of an
//! FTS `MATCH`, so it belongs behind the boundary. Phase 2 had storage hand
//! back every vector for the domain to score, which read as "keeping the
//! decision in the domain" but actually hardcoded brute-force scanning into
//! the interface — an ANN index cannot implement "give me all vectors"
//! usefully. Ranking and merging of results stay in the domain; finding
//! candidates does not.
//!
//! # Unsupported object types
//!
//! `ObjectType` declares six variants; only `File` and `Clipboard` have
//! storage today. Every operation on a type with no backing returns an
//! **error** — never an empty result. See [`ObjectStore::supported_types`].

use crate::object::{ObjectId, ObjectMetadata, ObjectPayload, ObjectType};
use anyhow::Result;

/// An object that hasn't been persisted yet, and therefore has no
/// [`ObjectId`]. Storage assigns the id and returns it.
///
/// This is deliberately not `Object`: an `Object` always has an identity,
/// and inventing a placeholder id for unsaved data (`0`, or an `Option`
/// nobody checks) is exactly the kind of quiet lie the domain model exists
/// to prevent.
#[derive(Debug, Clone)]
pub struct NewObject {
    pub metadata: ObjectMetadata,
    pub payload: ObjectPayload,
}

impl NewObject {
    pub fn new(metadata: ObjectMetadata, payload: ObjectPayload) -> Self {
        Self { metadata, payload }
    }

    pub fn object_type(&self) -> ObjectType {
        self.payload.object_type()
    }
}

/// A registered project folder. Returned as plain data so the caller can
/// run its own matching logic — see the module note on decisions.
#[derive(Debug, Clone)]
pub struct ProjectPathEntry {
    pub project_id: i64,
    pub path: String,
}

/// Persistence for objects, whatever their source.
///
/// Implementations dispatch on [`ObjectPayload`] internally. Callers never
/// do — that is the entire point of this trait.
pub trait ObjectStore {
    /// Object types this implementation can actually persist.
    ///
    /// Callers that iterate over types (Phase 4's generic search will) must
    /// use this rather than assuming every [`ObjectType`] variant is
    /// backed. Calling any other method with a type not in this list is a
    /// wiring bug and returns an error.
    fn supported_types(&self) -> &'static [ObjectType];

    /// Persists an object and returns its assigned identity.
    ///
    /// Whether this inserts or updates is a per-source rule the
    /// implementation owns: a file is upserted on its path (re-indexing an
    /// existing file must not duplicate it), a clipboard entry is always a
    /// new row (copying the same text twice is two events).
    fn save(&self, object: &NewObject) -> Result<ObjectId>;

    /// Deletes an object and everything derived from it — embeddings,
    /// index entries, anything else keyed on its identity. Enforcing that
    /// here rather than asking each caller to remember it is what makes
    /// `ARCHITECTURE_INVARIANTS.md` §10.2 an invariant instead of a
    /// convention.
    ///
    /// Returns whether a row actually existed.
    fn delete(&self, id: ObjectId) -> Result<bool>;

    /// Looks up an object by the natural key its source identifies it with
    /// — a file's path, later a screenshot's path or an email's message-id.
    ///
    /// Exists because deletion is driven by outside events that know a key,
    /// not an id: the filesystem watcher is told "this path disappeared"
    /// and has to turn that into an [`ObjectId`] to delete.
    ///
    /// Errors for types that have no natural key (a clipboard entry is not
    /// identified by its content — that is why deduplication compares
    /// against the most recent entry instead).
    fn resolve(&self, object_type: ObjectType, natural_key: &str) -> Result<Option<ObjectId>>;

    /// Text of the most recently stored object of this type.
    ///
    /// Exists for clipboard deduplication. Kept as an object-level
    /// operation rather than a clipboard-specific one because the same
    /// "did this just repeat?" check applies to browser visits and any
    /// other stream of near-duplicate events.
    fn latest_text(&self, object_type: ObjectType) -> Result<Option<String>>;
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

/// Vector storage and nearest-neighbour lookup.
///
/// Separate from [`ObjectStore`] because it is genuinely optional: with no
/// embedding model there are no vectors, and the rest of the system works
/// (`ARCHITECTURE_INVARIANTS.md` §8). A caller that only writes objects
/// should not have to name this trait.
///
/// Vector *deletion* is deliberately absent: it happens inside
/// [`ObjectStore::delete`], so derived data cannot outlive its object
/// through caller forgetfulness.
pub trait EmbeddingStore {
    fn put_vector(&self, id: ObjectId, vector: &[f32]) -> Result<()>;

    /// The `limit` nearest stored vectors of this type to `query`, best
    /// first, with their similarity scores.
    ///
    /// How that search is performed — today a brute-force scan, plausibly
    /// an ANN index once the corpus grows — is an implementation detail
    /// this signature does not constrain.
    fn nearest(
        &self,
        object_type: ObjectType,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(i64, f32)>>;

    /// Objects of this type that have no vector yet, paired with the text
    /// to embed. `limit` caps the batch; `usize::MAX` means "all".
    ///
    /// Returns text alongside ids so a backfill is one query rather than
    /// one query per row.
    fn pending_embeddings(
        &self,
        object_type: ObjectType,
        limit: usize,
    ) -> Result<Vec<(ObjectId, String)>>;
}
