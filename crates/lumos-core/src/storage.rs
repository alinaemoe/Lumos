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

use crate::object::{Object, ObjectId, ObjectMetadata, ObjectPayload, ObjectType};
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

    /// Objects of this type at or after `since` (unix seconds), most-recent
    /// first, capped to `limit` by storage — the text-less "today" /
    /// "yesterday" / "week" browse mode.
    ///
    /// This is deliberately its own method rather than a degenerate case
    /// of [`FullTextIndex::search_text`] or [`EmbeddingStore::nearest`]:
    /// there is no query to match or embed here, so it doesn't fit either
    /// trait's shape. "Recent" is a third, structurally distinct kind of
    /// candidate — see `ARCHITECTURE_INVARIANTS.md`'s Phase 4 entry for the
    /// conflict this resolves and why it isn't folded into RRF with the
    /// other two.
    ///
    /// What "most-recent" means is a per-type storage decision (a file's
    /// `modified_at` vs. a clipboard entry's insertion order) — the domain
    /// only asks for "recent objects of this type since this time," not
    /// which column that maps to.
    /// `until`, when present, is enforced by the implementation *before*
    /// `limit` is applied — not as a post-hydration filter. Objects outside
    /// `[since, until]` must never be able to consume the candidate budget
    /// that objects inside the window need: with `until: None` this method
    /// answers "the `limit` most recent objects since `since`," but with
    /// `until: Some(u)` it must answer "up to `limit` objects in
    /// `[since, u]`," and those are different queries, not the same query
    /// filtered afterward. (This was a real bug: an early version applied
    /// `until` only after this call's result was hydrated, so enough
    /// objects newer than `until` could exhaust `limit` before any object
    /// actually inside the window was ever fetched.)
    ///
    /// `detected_type`, when present, must be enforced the same way —
    /// before `limit`, not after. This was the identical bug in a second
    /// place: `type:jwt` with no other text means "list recent objects
    /// with `detected_type == "jwt"`," and if that constraint is only
    /// checked after this call returns, enough non-`jwt` rows newer than
    /// the actual `jwt` rows can fill `limit` before any `jwt` row is ever
    /// fetched — reproduced on the real live database (60-most-recent-id
    /// cutoff had moved past all three real `jwt` rows purely from
    /// unrelated clipboard growth). `detected_type` has no meaning for
    /// types that don't carry it (`File`) — implementations must ignore
    /// it for those, the same "no opinion, not an error" rule
    /// `matches_detected_type` already applies at the domain layer, not
    /// treat it as a reason to fail or to silently return nothing.
    fn recent(
        &self,
        object_type: ObjectType,
        since: i64,
        until: Option<i64>,
        detected_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ObjectId>>;

    /// Loads full objects by identity — the hydration step Search needs to
    /// turn an `EmbeddingStore::nearest`/`FullTextIndex::search_text` hit
    /// (a bare id) back into something with content and metadata.
    ///
    /// Added as prework for Phase 4 (Generic Search); nothing calls it yet
    /// — `search.rs` still hydrates via its own `file_by_id`/
    /// `clipboard::by_id` queries. Those are expected to be replaced by
    /// this method when the generic engine is actually built, not in this
    /// pass.
    ///
    /// Ids belonging to an unsupported type make the whole call fail
    /// (consistent with every other `ObjectStore` method) — but an id of a
    /// *supported* type that no longer exists (deleted between the index
    /// lookup and this call) is silently omitted from the result, not an
    /// error. A caller cannot distinguish "never existed" from "existed
    /// and was just deleted," and for hydration purposes it doesn't need
    /// to — either way there is nothing to show.
    ///
    /// Implementations must batch internally (one query per distinct
    /// `ObjectType` present in `ids`, not one query per id) — this is the
    /// operation a naive per-source `by_id` N+1 pattern was replaced for.
    fn load(&self, ids: &[ObjectId]) -> Result<Vec<Object>>;
}

/// Full-text candidate lookup — storage's side of FTS, the textual sibling
/// of [`EmbeddingStore::nearest`]. Storage decides *how* candidates are
/// found (SQLite FTS5 today, something else later); the domain decides
/// what to do with them once found (merge, rank, dedup — none of that is
/// implemented yet; see `ARCHITECTURE_INVARIANTS.md` §4).
///
/// Deliberately not folded into [`ObjectStore`]: a caller that only writes
/// or only does semantic search should not have to name this trait, the
/// same reasoning that keeps [`EmbeddingStore`] separate.
///
/// This does not yet replace `search.rs`'s own hand-written FTS queries —
/// those still exist, unchanged, alongside this trait. Wiring them together
/// is Phase 4's job; this trait only makes the capability reachable without
/// SQL outside `storage_sqlite.rs`.
pub trait FullTextIndex {
    /// FTS candidates for `query` within a single object type, best match
    /// first, paired with a relevance score where **higher is better** —
    /// the same convention `EmbeddingStore::nearest` uses for cosine
    /// similarity. (SQLite FTS5's native `rank` is the opposite — smaller
    /// is more relevant — so the implementation negates it before
    /// returning; see `storage_sqlite.rs`.)
    ///
    /// That shared convention does *not* mean the two scores are on the
    /// same scale — bm25-derived relevance and cosine similarity are not
    /// comparable magnitudes, only comparable in *direction*. Fusing them
    /// into one ranking (RRF or otherwise) is explicitly Phase 4's
    /// decision, not something this trait resolves.
    fn search_text(&self, object_type: ObjectType, query: &str, limit: usize) -> Result<Vec<(i64, f32)>>;
}

pub trait ProjectStore {
    fn get_or_create_project(&self, name: &str) -> Result<i64>;
    /// Read-only lookup — the counterpart to [`ProjectStore::get_or_create_project`]
    /// for callers that must never create a project as a side effect of
    /// looking one up.
    ///
    /// Exists for Phase 5's LLM boundary specifically: an interpreter may
    /// return a project *name* it heard in a query (§5.1 forbids it from
    /// ever returning an id), and resolving that name against a
    /// non-existent or misspelled project must degrade to "no project
    /// filter", never silently create an empty project as a side effect of
    /// a wrong guess. `get_or_create_project` is unsafe for that caller for
    /// exactly this reason — it was written for `project add`, where
    /// "create if missing" is the correct and intended behavior, not for
    /// resolving untrusted free-text.
    fn find_project_by_name(&self, name: &str) -> Result<Option<i64>>;
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

    /// How many vectors of this type are stored.
    ///
    /// Needed by the domain's relevance judgment: deciding whether a
    /// query's top similarity score is a genuine outlier or merely the
    /// maximum of a flat, uninformative distribution requires knowing what
    /// fraction of the corpus sits near that top — which needs the corpus
    /// size as a denominator. Counting rows is a storage question, not a
    /// relevance judgment, so it belongs here; the judgment itself stays in
    /// `embeddings.rs`.
    fn vector_count(&self, object_type: ObjectType) -> Result<usize>;

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
