//! The unified Object model — see `ARCHITECTURE_INVARIANTS.md` §1.
//!
//! Pure domain layer: no `rusqlite`, no filesystem APIs, no Tauri, no UI.
//! Every current and future data source (File, Clipboard, Screenshot,
//! BrowserVisit, Session, Email) is representable as an `Object`. Nothing in
//! this module knows how an `Object` is stored or how it was produced —
//! that's the job of Phase 2 (storage boundary) and Phase 3 (ingestion
//! pipeline), respectively. This phase only introduces the vocabulary.

use serde::Serialize;

/// Which kind of source produced an object. New sources add a variant here
/// — nothing else in this module changes shape as a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    File,
    Clipboard,
    Screenshot,
    BrowserVisit,
    Session,
    Email,
}

/// Global identity for an object: its type plus a locally-unique id within
/// that type. This is deliberately *not* one global counter across types —
/// today's storage (Phase 2+) already hands out per-table row ids, and a
/// composite key lets that continue without forcing a schema change just to
/// introduce this model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ObjectId {
    pub object_type: ObjectType,
    pub local_id: i64,
}

impl ObjectId {
    pub fn new(object_type: ObjectType, local_id: i64) -> Self {
        Self { object_type, local_id }
    }
}

/// Fields every object has, regardless of source. Everything here is
/// `Option` on purpose: this phase adapts from *existing* query results
/// (see `object_adapters.rs`), and today's `files`/`clipboard` read paths
/// don't project every column that exists in storage (e.g. a file's
/// `project_id` isn't selected by `search::search()` today — only its
/// `project_name` is). Adapters must not fabricate data that isn't
/// actually available; a later phase that extends storage access can
/// tighten these to non-optional once the data is reliably there.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ObjectMetadata {
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub project_id: Option<i64>,
    /// Free-form provenance, e.g. the foreground app a clipboard entry was
    /// copied from. Not every source has one.
    pub source: Option<String>,
}

/// A file on disk, as far as the domain model is concerned. Matches what
/// `search::SearchResult` carries today, plus `project_name` — which is
/// file-specific *display* data (not a stable identity like `project_id`),
/// so it lives on the payload rather than in `ObjectMetadata`.
#[derive(Debug, Clone, Serialize)]
pub struct FilePayload {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size: i64,
    /// Content hash. `None` both for files too large to hash and for
    /// conversions from sources that don't carry it (see the `SearchResult`
    /// adapter, whose query doesn't select this column).
    pub hash: Option<String>,
    pub project_name: Option<String>,
}

/// A clipboard history entry, as far as the domain model is concerned.
/// Matches what `clipboard::ClipboardResult` carries today.
#[derive(Debug, Clone, Serialize)]
pub struct ClipboardPayload {
    pub text_content: String,
    pub detected_type: String,
    pub pinned: bool,
}

/// Placeholder — no Screenshot pipeline exists yet (see
/// `LUMOS_DESIGN.md`/`ARCHITECTURE_INVARIANTS.md` §12). Declared now so the
/// `ObjectType`/`ObjectPayload` vocabulary is already complete; fields are
/// added when the real ingestion pipeline for this source is built
/// (Phase 3/6), not guessed at here.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScreenshotPayload {}

/// Placeholder — no browser-history pipeline exists yet.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BrowserVisitPayload {}

/// Placeholder — no Session pipeline exists yet.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionPayload {}

/// Placeholder — no Email pipeline exists yet.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EmailPayload {}

/// Source-specific content. Only the owning source's adapter/pipeline
/// stage is meant to construct or interpret a given variant — cross-
/// cutting code (Search, ranking, dedup, UI-shell) must not match on this
/// beyond routing to a renderer (see `ARCHITECTURE_INVARIANTS.md` §4.1).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectPayload {
    File(FilePayload),
    Clipboard(ClipboardPayload),
    Screenshot(ScreenshotPayload),
    BrowserVisit(BrowserVisitPayload),
    Session(SessionPayload),
    Email(EmailPayload),
}

impl ObjectPayload {
    pub fn object_type(&self) -> ObjectType {
        match self {
            ObjectPayload::File(_) => ObjectType::File,
            ObjectPayload::Clipboard(_) => ObjectType::Clipboard,
            ObjectPayload::Screenshot(_) => ObjectType::Screenshot,
            ObjectPayload::BrowserVisit(_) => ObjectType::BrowserVisit,
            ObjectPayload::Session(_) => ObjectType::Session,
            ObjectPayload::Email(_) => ObjectType::Email,
        }
    }
}

/// One unified representation of anything Lumos has ingested.
#[derive(Debug, Clone, Serialize)]
pub struct Object {
    pub id: ObjectId,
    pub metadata: ObjectMetadata,
    pub payload: ObjectPayload,
}

impl Object {
    pub fn new(local_id: i64, metadata: ObjectMetadata, payload: ObjectPayload) -> Self {
        Self {
            id: ObjectId::new(payload.object_type(), local_id),
            metadata,
            payload,
        }
    }
}

/// How two objects relate to each other. Not persisted anywhere yet — no
/// `session_events`-equivalent storage exists (see
/// `ARCHITECTURE_INVARIANTS.md` §1.4/§12). Introduced now purely as
/// vocabulary so Phase 3+ has a name for this instead of inventing one
/// per source when Sessions/relations actually get built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    BelongsTo,
    References,
    DerivedFrom,
    Custom(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Relation {
    pub from: ObjectId,
    pub to: ObjectId,
    pub relation_type: RelationType,
}
