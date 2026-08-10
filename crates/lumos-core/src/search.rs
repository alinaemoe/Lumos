//! Generic search — see `ARCHITECTURE_INVARIANTS.md` §4 and its Phase 4
//! entries for the design history.
//!
//! `search_all` (the engine real callers use) never names `files`,
//! `clipboard_entries`, `files_fts`, `clipboard_fts`, or `clipboard::search`
//! — it retrieves candidates through [`FullTextIndex`], [`EmbeddingStore`],
//! and [`ObjectStore::recent`], merges them by [`ObjectId`], fuses FTS and
//! semantic rank via Reciprocal Rank Fusion, hydrates through
//! [`ObjectStore::load`] in one batched call, and only then applies the
//! filters/ranking signals that need real object data. `rusqlite` does not
//! appear anywhere in this file.
//!
//! `search()` is a separate, deliberately smaller thing — the CLI's raw
//! FTS-only smoke-test command (`lumos search <query>`), unchanged in
//! scope from before Phase 4 (still no semantic pass, still no RRF). It's
//! rewritten here only to the extent needed to also drop `Connection` —
//! see its doc comment.

use crate::clipboard::{ClipboardResult, Filters};
use crate::embeddings;
use crate::object::{Object, ObjectId, ObjectPayload, ObjectType};
use crate::storage::{EmbeddingStore, FullTextIndex, ObjectStore};
use anyhow::Result;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchHit {
    File(SearchResult),
    Clipboard(ClipboardResult),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size: i64,
    pub project_name: Option<String>,
}

/// Raw user input can't go straight into an FTS5 MATCH expression — FTS5
/// treats `. - " * : ( )` etc. as query syntax, so a literal search for
/// "CheckoutItem.cs" throws a syntax error instead of just searching for it.
/// Wrapping each word as an escaped quoted phrase forces FTS5 to treat it as
/// literal text; juxtaposed quoted phrases default to AND between them.
///
/// This exact escaping rule is duplicated in `storage_sqlite.rs` — see that
/// copy's doc comment for why (infrastructure shouldn't import from here).
pub(crate) fn sanitize_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Pulls `type:xxx` / `today` / `yesterday` / `week` tokens out of the raw
/// query string, leaving whatever's left as the free-text portion. These
/// are relative-hours windows (24h / 48h / 7d back from now), not calendar-
/// day boundaries — exact "midnight to midnight" would need timezone
/// handling we don't have; "at least this recent" is close enough and a lot
/// simpler to reason about (each filter is a strict superset of the
/// previous one, so there's no surprising off-by-one at day boundaries).
///
/// Kept as its own function (rather than folded into `parse_intent`)
/// because `clipboard::search`/the CLI's standalone `clipboard search`
/// command call it directly against `Filters`, independent of the generic
/// engine — Phase 4 didn't touch that path.
pub fn parse_filters(raw: &str) -> (String, Filters) {
    let mut filters = Filters::default();
    let mut remaining = Vec::new();

    for token in raw.split_whitespace() {
        let lower = token.to_lowercase();
        if let Some(rest) = lower.strip_prefix("type:") {
            filters.detected_type = Some(rest.to_string());
        } else if lower == "today" {
            filters.since = Some(now_ts() - 86_400);
        } else if lower == "yesterday" {
            filters.since = Some(now_ts() - 2 * 86_400);
        } else if lower == "week" {
            filters.since = Some(now_ts() - 7 * 86_400);
        } else {
            remaining.push(token);
        }
    }

    (remaining.join(" "), filters)
}

/// What the generic search engine actually consumes. Replaces the old
/// `(String, Filters)` tuple with a named type per Phase 4.
///
/// `object_types` and `detected_type` are unrelated axes that happen to
/// share the word "type" in casual conversation — see the long note on
/// `ObjectType` in `object.rs`. `detected_type` here means exactly what it
/// always has (`type:jwt`, clipboard-only); `object_types` is new — which
/// *sources* to search at all.
#[derive(Debug, Clone)]
pub struct SearchIntent {
    pub text: String,
    pub object_types: Vec<ObjectType>,
    pub detected_type: Option<String>,
    pub since: Option<i64>,
    /// No query syntax sets this yet — always `None` from `parse_intent`.
    /// Present now because the ranking function (`project_boost`) already
    /// needs somewhere to read a project filter from once one exists,
    /// and adding the field later would mean touching every call site
    /// again. Wiring an actual filter syntax for it is not this phase's
    /// job.
    pub project_id: Option<i64>,
}

/// Builds a [`SearchIntent`] from raw query text using the existing
/// `type:`/`today`/`yesterday`/`week` parser.
///
/// `object_types` reproduces the exact restriction `search_all` applied
/// before Phase 4: a `detected_type` filter means "clipboard only" (files
/// have no `detected_type`, and searching `type:jwt` against filenames was
/// already documented as "just confusing" pre-Phase-4) — everything else
/// searches both sources known today.
pub fn parse_intent(raw: &str) -> SearchIntent {
    let (text, filters) = parse_filters(raw);
    let object_types = if filters.detected_type.is_some() {
        vec![ObjectType::Clipboard]
    } else {
        vec![ObjectType::File, ObjectType::Clipboard]
    };
    SearchIntent {
        text,
        object_types,
        detected_type: filters.detected_type,
        since: filters.since,
        project_id: None,
    }
}

/// Which retrieval method produced a candidate. Purely an internal
/// bookkeeping detail for merge/RRF — never exposed outside this module,
/// and deliberately not a public "SearchObject"-style domain type (no
/// second consumer exists to justify one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetrievalSource {
    FullText,
    Semantic,
    /// The text-less recency browse mode (`ObjectStore::recent`). Never
    /// participates in RRF — see `merge_candidates`.
    Recency,
}

/// One candidate from one retrieval method, before merge. `rank` is
/// 1-based position within its own `(object_type, source)` list — the only
/// thing RRF needs; the raw FTS/cosine score is intentionally not carried
/// past this point; §6 of the Phase 4 brief was explicit that those scores
/// must never be summed directly (different scales), and rank position is
/// all RRF operates on.
#[derive(Debug, Clone)]
struct Candidate {
    object_id: ObjectId,
    source: RetrievalSource,
    rank: usize,
}

/// How many candidates to request per `(object_type, source)` list, beyond
/// the final result `limit`. Needed because `since`/`detected_type`
/// filtering now happens *after* retrieval (storage's `search_text`/
/// `nearest` have no filter parameters — see the Phase 4 conflict note in
/// `ARCHITECTURE_INVARIANTS.md`), so retrieving exactly `limit` candidates
/// and then filtering some out could under-fill the final result. This is
/// an approximation, not a guarantee: an adversarial case (every one of the
/// top `limit * CANDIDATE_OVERFETCH` hits filtered out, while an older
/// match beyond that window would have passed) can still return fewer
/// results than the pre-Phase-4 SQL-side filtering would have. Accepted
/// deliberately rather than solved — see the same invariants entry.
const CANDIDATE_OVERFETCH: usize = 3;

/// Reciprocal Rank Fusion constant. Standard choice (also used in the
/// original design doc's own plan for this); large enough that rank 1 vs.
/// rank 2 isn't a wild swing, small enough that being on a list at all
/// still matters. Fixed here, in one place, per the brief's explicit
/// requirement not to scatter this as a magic number.
const RRF_K: f32 = 60.0;

fn rrf_score(rank: usize) -> f32 {
    1.0 / (RRF_K + rank as f32)
}

/// Ranking-signal weights. All deliberately small and *not* independently
/// tuned — RRF-fused scores from `rrf_score` above are themselves tiny
/// (on the order of 1/60 ≈ 0.016 per contributing list), so these act as
/// tie-breaking nudges on top of retrieval relevance, not competing
/// signals of equal weight. This is a first-pass calibration, stated
/// honestly as one: revisit with real usage data before trusting it beyond
/// "recent/exact/same-project nudges slightly ahead of otherwise-equal
/// results."
const RECENCY_BOOST_MAX: f32 = 0.01;
const EXACT_MATCH_BOOST: f32 = 0.02;
const PARTIAL_MATCH_BOOST: f32 = 0.005;
const PROJECT_MATCH_BOOST: f32 = 0.005;

/// Retrieval: for each object type in the intent, pull FTS + semantic
/// candidates (if there's text) or recency candidates (if there's a
/// `since` and no text). No `match ObjectType { File => .., Clipboard =>
/// .. }` here — the loop is the same for every type in `object_types`;
/// only the trait calls it makes are type-parameterized, and those
/// dispatch inside `storage_sqlite.rs`, not here.
fn retrieve_candidates<S>(store: &S, intent: &SearchIntent, limit: usize) -> Result<Vec<Candidate>>
where
    S: FullTextIndex + EmbeddingStore + ObjectStore,
{
    let fetch_n = (limit * CANDIDATE_OVERFETCH).max(limit);
    let has_text = !intent.text.trim().is_empty();
    let mut candidates = Vec::new();

    for &object_type in &intent.object_types {
        if has_text {
            for (rank, (local_id, _score)) in store
                .search_text(object_type, &intent.text, fetch_n)?
                .into_iter()
                .enumerate()
            {
                candidates.push(Candidate {
                    object_id: ObjectId::new(object_type, local_id),
                    source: RetrievalSource::FullText,
                    rank: rank + 1,
                });
            }
            for (rank, (local_id, _score)) in
                embeddings::search(store, object_type, &intent.text, fetch_n)?
                    .into_iter()
                    .enumerate()
            {
                candidates.push(Candidate {
                    object_id: ObjectId::new(object_type, local_id),
                    source: RetrievalSource::Semantic,
                    rank: rank + 1,
                });
            }
        } else if intent.since.is_some() || intent.detected_type.is_some() {
            // No text to match/embed — this is a listing, not a search.
            // Triggered by `since` ("today"/"week") *or* `detected_type`
            // alone ("type:jwt" with nothing else): pre-Phase-4
            // `clipboard::search`'s text-less branch fired on either
            // condition (`if !has_text && detected_type.is_none() &&
            // since.is_none() { return empty }`), not `since` only —
            // missing this the first time through produced `type:jwt`
            // returning zero results, caught by the CLI smoke test rather
            // than left as a silent regression. `since` defaults to
            // "the beginning of time" so a `detected_type`-only intent
            // still lists everything of that type; `matches_since`/
            // `matches_detected_type` after hydration narrow it down.
            let since = intent.since.unwrap_or(i64::MIN);
            for (rank, object_id) in store.recent(object_type, since, fetch_n)?.into_iter().enumerate() {
                candidates.push(Candidate {
                    object_id,
                    source: RetrievalSource::Recency,
                    rank: rank + 1,
                });
            }
        }
    }

    Ok(candidates)
}

/// Merge + RRF + dedup in one pass: folding into a `HashMap<ObjectId, f32>`
/// *is* the dedup (a candidate found by both FTS and semantic collapses to
/// one entry, with both contributions summed — which is standard RRF
/// behavior: appearing on multiple lists should score higher, not just
/// "count once"). `File(42)` and `Clipboard(42)` are different keys because
/// `ObjectId` carries `object_type` — never just the bare `i64`.
///
/// Recency-sourced candidates contribute `0.0` here on purpose (§6 of the
/// brief: recency mode is not RRF'd against anything) but still occupy a
/// map entry, so they survive to hydration/ranking, where the recency
/// ranking signal (not RRF) is what actually orders them.
fn merge_candidates(candidates: Vec<Candidate>) -> HashMap<ObjectId, f32> {
    let mut fused: HashMap<ObjectId, f32> = HashMap::new();
    for c in candidates {
        let contribution = match c.source {
            RetrievalSource::FullText | RetrievalSource::Semantic => rrf_score(c.rank),
            RetrievalSource::Recency => 0.0,
        };
        *fused.entry(c.object_id).or_insert(0.0) += contribution;
    }
    fused
}

/// The one place generic search reads a `Clipboard` payload field —
/// documented bounded exception, not a template for future filters. `type:`
/// has meant "clipboard's detected_type" since before Phase 4; reproducing
/// that exactly requires this check to exist somewhere, and post-hydration
/// is the only point at which the data is available at all (see the
/// module's `CANDIDATE_OVERFETCH` doc). Non-`Clipboard` objects pass
/// through untouched — by the time this runs, `intent.object_types` has
/// already excluded files whenever `detected_type` is set (`parse_intent`),
/// so in current usage this never actually has to exclude a non-Clipboard
/// object; the default is `true` so that stays correct if that ever
/// changes rather than silently start dropping objects it has no opinion
/// about.
fn matches_detected_type(object: &Object, wanted: &Option<String>) -> bool {
    let Some(wanted) = wanted else { return true };
    match &object.payload {
        ObjectPayload::Clipboard(c) => &c.detected_type == wanted,
        _ => true,
    }
}

/// Generic — reads only `ObjectMetadata`, never payload. `updated_at` is
/// used if present, else `created_at`. This one rule reproduces both old
/// per-type behaviors exactly without a `match ObjectType`: a file's
/// `updated_at` is its `modified_at` (what the old code filtered files on),
/// a clipboard entry has no `updated_at` so this falls back to
/// `created_at` (what the old code filtered clipboard on).
fn matches_since(object: &Object, since: Option<i64>) -> bool {
    let Some(since) = since else { return true };
    object
        .metadata
        .updated_at
        .or(object.metadata.created_at)
        .map(|ts| ts >= since)
        .unwrap_or(false)
}

/// Same "recency timestamp" convention as `matches_since` — generic,
/// metadata-only. Simple decay so results within the last day or so get
/// most of the available boost and it tapers off; not meant to be more
/// precise than that.
fn recency_boost(object: &Object, now: i64) -> f32 {
    let Some(ts) = object.metadata.updated_at.or(object.metadata.created_at) else {
        return 0.0;
    };
    let age_days = ((now - ts).max(0) as f32) / 86_400.0;
    RECENCY_BOOST_MAX / (1.0 + age_days)
}

/// The second (and last) bounded payload exception in this module, same
/// class as `matches_detected_type`: computing "is this an exact/partial
/// match" inherently requires looking at content, which for `File` is
/// `filename` and for `Clipboard` is `text_content`. Other payload
/// variants contribute no boost — not an oversight, just nothing to
/// compare against yet.
fn exact_match_boost(object: &Object, query_text: &str) -> f32 {
    let needle = query_text.trim().to_lowercase();
    if needle.is_empty() {
        return 0.0;
    }
    let haystack = match &object.payload {
        ObjectPayload::File(f) => f.filename.to_lowercase(),
        ObjectPayload::Clipboard(c) => c.text_content.to_lowercase(),
        _ => return 0.0,
    };
    if haystack == needle {
        EXACT_MATCH_BOOST
    } else if haystack.contains(&needle) {
        PARTIAL_MATCH_BOOST
    } else {
        0.0
    }
}

/// Generic — `ObjectMetadata::project_id` only. Always `0.0` today since
/// `parse_intent` never sets `SearchIntent::project_id`; wired now so
/// adding that filter syntax later doesn't require touching ranking too.
fn project_boost(object: &Object, wanted_project_id: Option<i64>) -> f32 {
    match (object.metadata.project_id, wanted_project_id) {
        (Some(a), Some(b)) if a == b => PROJECT_MATCH_BOOST,
        _ => 0.0,
    }
}

/// **Pin/boost is deliberately absent from ranking.** Not an oversight —
/// see `ARCHITECTURE_INVARIANTS.md`'s Phase 4 entry: `pinned` lives on
/// `ClipboardPayload`, not `ObjectMetadata`, and generic ranking reading
/// payload for this would need the same kind of bounded exception as
/// `matches_detected_type`/`exact_match_boost` above, but *this specific
/// one* was called out by the brief as needing an explicit model decision
/// (promote to metadata vs. stay source-specific) before implementation,
/// not as this phase's call to make unilaterally. The pre-Phase-4 engine
/// never ranked by pin either, so this is not a behavior regression —
/// only a signal this phase declines to add.
const _PIN_RANKING_NOTE: () = ();

/// Turns a hydrated `Object` back into the shape the frontend already
/// understands. `search.rs` only ever retrieves `File`/`Clipboard` objects
/// today (`parse_intent` never produces another `ObjectType`), so any other
/// variant reaching here is a real bug — a future object type added to
/// `object_types` without updating this adapter — not a condition a user
/// can trigger, hence `unreachable!` rather than a swallowed `None`.
fn to_search_hit(object: Object) -> SearchHit {
    match object.payload {
        ObjectPayload::File(f) => SearchHit::File(SearchResult {
            id: object.id.local_id,
            path: f.path,
            filename: f.filename,
            extension: f.extension,
            size: f.size,
            project_name: f.project_name,
        }),
        ObjectPayload::Clipboard(c) => SearchHit::Clipboard(ClipboardResult {
            id: object.id.local_id,
            text_content: c.text_content,
            detected_type: c.detected_type,
            source_app: object.metadata.source,
            pinned: c.pinned,
            created_at: object.metadata.created_at.unwrap_or(0),
        }),
        other => unreachable!(
            "search_all only ever retrieves File/Clipboard objects; got {:?}",
            other.object_type()
        ),
    }
}

/// The generic engine: `SearchIntent` → candidates → merge/RRF → hydrate →
/// filter → rank → limit → frontend-compatible results. This is the
/// function both the CLI's `search-all` and the Tauri `search_files`
/// command call — see their call sites for how a `Connection` becomes the
/// generic `S` this takes.
pub fn search_all<S>(store: &S, raw_query: &str, limit: i64) -> Result<Vec<SearchHit>>
where
    S: FullTextIndex + EmbeddingStore + ObjectStore,
{
    let intent = parse_intent(raw_query);
    let limit = limit.max(0) as usize;
    if limit == 0 {
        return Ok(Vec::new());
    }

    let candidates = retrieve_candidates(store, &intent, limit)?;
    let fused = merge_candidates(candidates);

    let ids: Vec<ObjectId> = fused.keys().copied().collect();
    let mut objects: HashMap<ObjectId, Object> =
        store.load(&ids)?.into_iter().map(|o| (o.id, o)).collect();

    let now = crate::now_secs();
    let mut ranked: Vec<(f32, Object)> = Vec::with_capacity(fused.len());
    for (id, retrieval_score) in fused {
        // A candidate id can be missing from `objects` if it was deleted
        // between being found and being loaded — `ObjectStore::load`
        // already treats that as "omit, don't error" (see its doc), and
        // this follows the same rule rather than treating it as a bug.
        let Some(object) = objects.remove(&id) else { continue };
        if !matches_since(&object, intent.since) {
            continue;
        }
        if !matches_detected_type(&object, &intent.detected_type) {
            continue;
        }
        let score = retrieval_score
            + recency_boost(&object, now)
            + exact_match_boost(&object, &intent.text)
            + project_boost(&object, intent.project_id);
        ranked.push((score, object));
    }

    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.truncate(limit);

    Ok(ranked.into_iter().map(|(_, obj)| to_search_hit(obj)).collect())
}

/// The CLI's raw FTS-only smoke-test command (`lumos search <query>`) —
/// distinct from `search_all` before Phase 4 and kept distinct after it:
/// no semantic pass, no RRF, files only, by design (its own doc comment
/// always called it a smoke test, not the real search path). Rewritten
/// only enough to drop `Connection`, via the same `FullTextIndex`/
/// `ObjectStore` traits `search_all` uses.
pub fn search<S>(store: &S, query: &str, since: Option<i64>, limit: i64) -> Result<Vec<SearchResult>>
where
    S: FullTextIndex + ObjectStore,
{
    let text = query.trim();
    let limit = limit.max(0) as usize;
    if limit == 0 || (text.is_empty() && since.is_none()) {
        return Ok(Vec::new());
    }

    let ids: Vec<ObjectId> = if !text.is_empty() {
        store
            .search_text(ObjectType::File, text, limit)?
            .into_iter()
            .map(|(local_id, _score)| ObjectId::new(ObjectType::File, local_id))
            .collect()
    } else if let Some(since) = since {
        store.recent(ObjectType::File, since, limit)?
    } else {
        Vec::new()
    };

    // `load` doesn't promise to preserve input order, so it's restored
    // explicitly — `search_text`/`recent` already returned best-first.
    let mut by_id: HashMap<ObjectId, Object> =
        store.load(&ids)?.into_iter().map(|o| (o.id, o)).collect();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(Object { payload: ObjectPayload::File(f), .. }) = by_id.remove(&id) {
            out.push(SearchResult {
                id: id.local_id,
                path: f.path,
                filename: f.filename,
                extension: f.extension,
                size: f.size,
                project_name: f.project_name,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{ClipboardPayload, FilePayload, ObjectMetadata};
    use crate::storage::{NewObject, ProjectStore};
    use crate::storage_sqlite::SqliteStore;

    fn store_conn() -> rusqlite::Connection {
        crate::db::open_in_memory().unwrap()
    }

    fn file_at(path: &str, filename: &str, modified_at: i64, project_id: Option<i64>) -> NewObject {
        NewObject::new(
            ObjectMetadata { created_at: Some(modified_at), updated_at: Some(modified_at), project_id, source: None },
            ObjectPayload::File(FilePayload {
                path: path.into(),
                filename: filename.into(),
                extension: Some("rs".into()),
                size: 10,
                hash: None,
                project_name: None,
            }),
        )
    }

    fn clip_at(text: &str, detected_type: &str, created_at: i64) -> NewObject {
        NewObject::new(
            ObjectMetadata { created_at: Some(created_at), updated_at: None, project_id: None, source: None },
            ObjectPayload::Clipboard(ClipboardPayload {
                text_content: text.into(),
                detected_type: detected_type.into(),
                pinned: false,
                search_extra: None,
            }),
        )
    }

    // ---- Retrieval ----

    #[test]
    fn retrieval_finds_file_via_fts() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&file_at("C:/p/hydration_notes.rs", "hydration_notes.rs", 100, None)).unwrap();

        let intent = SearchIntent { text: "hydration_notes".into(), object_types: vec![ObjectType::File], detected_type: None, since: None, project_id: None };
        let candidates = retrieve_candidates(&st, &intent, 10).unwrap();
        assert!(candidates.iter().any(|c| c.object_id == id && c.source == RetrievalSource::FullText));
    }

    #[test]
    fn retrieval_finds_clipboard_via_fts() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&clip_at("SELECT * FROM users", "sql", 100)).unwrap();

        let intent = SearchIntent { text: "users".into(), object_types: vec![ObjectType::Clipboard], detected_type: None, since: None, project_id: None };
        let candidates = retrieve_candidates(&st, &intent, 10).unwrap();
        assert!(candidates.iter().any(|c| c.object_id == id && c.source == RetrievalSource::FullText));
    }

    #[test]
    fn retrieval_semantic_is_gracefully_empty_without_a_model_but_fts_still_works() {
        // No network/model in this environment - embeddings::search() is a
        // guaranteed no-op. This is exactly the "embeddings unavailable ->
        // pipeline continues" acceptance path, at the retrieval level.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&file_at("C:/p/main.rs", "main.rs", 100, None)).unwrap();

        let intent = SearchIntent { text: "main".into(), object_types: vec![ObjectType::File], detected_type: None, since: None, project_id: None };
        let candidates = retrieve_candidates(&st, &intent, 10).unwrap();
        assert!(candidates.iter().any(|c| c.object_id == id && c.source == RetrievalSource::FullText));
        assert!(
            candidates.iter().all(|c| c.source != RetrievalSource::Semantic),
            "no model available in this environment, so no Semantic candidates should appear"
        );
    }

    #[test]
    fn retrieval_recency_mode_fires_on_since_alone() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&file_at("C:/p/a.rs", "a.rs", 500, None)).unwrap();

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::File], detected_type: None, since: Some(100), project_id: None };
        let candidates = retrieve_candidates(&st, &intent, 10).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].object_id, id);
        assert_eq!(candidates[0].source, RetrievalSource::Recency);
    }

    #[test]
    fn retrieval_recency_mode_fires_on_detected_type_alone_no_since() {
        // Regression coverage for the bug the CLI smoke test caught:
        // `type:jwt` with nothing else must still list matching entries.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&clip_at("eyJ.eyJ.sig", "jwt", 100)).unwrap();

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::Clipboard], detected_type: Some("jwt".into()), since: None, project_id: None };
        let candidates = retrieve_candidates(&st, &intent, 10).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].object_id, id);
        assert_eq!(candidates[0].source, RetrievalSource::Recency);
    }

    #[test]
    fn retrieval_empty_text_and_no_filters_yields_no_candidates() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&file_at("C:/p/a.rs", "a.rs", 100, None)).unwrap();

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::File], detected_type: None, since: None, project_id: None };
        assert!(retrieve_candidates(&st, &intent, 10).unwrap().is_empty());
    }

    #[test]
    fn retrieval_unsupported_object_type_propagates_error() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let intent = SearchIntent { text: "x".into(), object_types: vec![ObjectType::Screenshot], detected_type: None, since: None, project_id: None };
        assert!(retrieve_candidates(&st, &intent, 10).is_err());
    }

    // ---- Merge / RRF / dedup ----

    #[test]
    fn rrf_favors_better_rank() {
        assert!(rrf_score(1) > rrf_score(2));
        assert!(rrf_score(2) > rrf_score(10));
    }

    #[test]
    fn merge_fuses_scores_for_same_object_found_by_both_methods() {
        let id = ObjectId::new(ObjectType::File, 1);
        let other = ObjectId::new(ObjectType::File, 2);
        let candidates = vec![
            Candidate { object_id: id, source: RetrievalSource::FullText, rank: 1 },
            Candidate { object_id: id, source: RetrievalSource::Semantic, rank: 1 },
            Candidate { object_id: other, source: RetrievalSource::FullText, rank: 1 },
        ];
        let fused = merge_candidates(candidates);
        assert!(
            fused[&id] > fused[&other],
            "an object found by two retrieval methods must score higher than one found by only one"
        );
    }

    #[test]
    fn merge_dedups_by_object_id_not_bare_local_id() {
        let file_42 = ObjectId::new(ObjectType::File, 42);
        let clip_42 = ObjectId::new(ObjectType::Clipboard, 42);
        let candidates = vec![
            Candidate { object_id: file_42, source: RetrievalSource::FullText, rank: 1 },
            Candidate { object_id: clip_42, source: RetrievalSource::FullText, rank: 1 },
        ];
        let fused = merge_candidates(candidates);
        assert_eq!(fused.len(), 2, "File(42) and Clipboard(42) are different objects, not a dedup collision");
    }

    #[test]
    fn merge_recency_candidates_contribute_zero_to_rrf() {
        let id = ObjectId::new(ObjectType::File, 1);
        let fused = merge_candidates(vec![Candidate { object_id: id, source: RetrievalSource::Recency, rank: 1 }]);
        assert_eq!(fused[&id], 0.0, "recency mode is not fused via RRF against anything");
    }

    #[test]
    fn merge_of_semantic_only_candidate_with_no_fts_match_still_survives() {
        // Simulates what a real embedding model would produce, without
        // depending on one being available in this environment (see
        // `retrieval_semantic_is_gracefully_empty_without_a_model...`
        // above for why the real `embeddings::search` path can't be
        // exercised end-to-end here).
        let semantic_only = ObjectId::new(ObjectType::Clipboard, 7);
        let fused = merge_candidates(vec![Candidate { object_id: semantic_only, source: RetrievalSource::Semantic, rank: 1 }]);
        assert!(fused.contains_key(&semantic_only));
        assert!(fused[&semantic_only] > 0.0);
    }

    // ---- Generic filters / signals ----

    #[test]
    fn matches_since_uses_updated_at_then_falls_back_to_created_at() {
        let file_obj = Object::new(1, ObjectMetadata { created_at: Some(1), updated_at: Some(500), project_id: None, source: None }, ObjectPayload::File(FilePayload { path: "x".into(), filename: "x".into(), extension: None, size: 0, hash: None, project_name: None }));
        assert!(matches_since(&file_obj, Some(500)));
        assert!(!matches_since(&file_obj, Some(501)));

        let clip_obj = Object::new(1, ObjectMetadata { created_at: Some(500), updated_at: None, project_id: None, source: None }, ObjectPayload::Clipboard(ClipboardPayload { text_content: "x".into(), detected_type: "text".into(), pinned: false, search_extra: None }));
        assert!(matches_since(&clip_obj, Some(500)), "clipboard has no updated_at, must fall back to created_at");
    }

    #[test]
    fn matches_detected_type_is_a_noop_for_non_clipboard_objects() {
        let file_obj = Object::new(1, ObjectMetadata::default(), ObjectPayload::File(FilePayload { path: "x".into(), filename: "x".into(), extension: None, size: 0, hash: None, project_name: None }));
        assert!(matches_detected_type(&file_obj, &Some("jwt".into())), "a File is never excluded by a detected_type filter it has no opinion on");
    }

    #[test]
    fn exact_match_outranks_partial_which_outranks_none() {
        let obj = |name: &str| Object::new(1, ObjectMetadata::default(), ObjectPayload::File(FilePayload { path: name.into(), filename: name.into(), extension: None, size: 0, hash: None, project_name: None }));
        let exact = exact_match_boost(&obj("main.rs"), "main.rs");
        let partial = exact_match_boost(&obj("main.rs.bak"), "main.rs");
        let none = exact_match_boost(&obj("readme.md"), "main.rs");
        assert!(exact > partial);
        assert!(partial > none);
        assert_eq!(none, 0.0);
    }

    // ---- Full pipeline (search_all) ----

    #[test]
    fn search_all_free_text_returns_mixed_file_and_clipboard() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&file_at("C:/p/hydration_notes.rs", "hydration_notes.rs", 100, None)).unwrap();
        st.save(&clip_at("hydration failed in react", "text", 200)).unwrap();

        let hits = search_all(&st, "hydration", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| matches!(h, SearchHit::File(_))));
        assert!(hits.iter().any(|h| matches!(h, SearchHit::Clipboard(_))));
    }

    #[test]
    fn search_all_exact_match_ranks_above_partial_match() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&clip_at("react", "text", 100)).unwrap();
        st.save(&clip_at("a note about react hooks", "text", 100)).unwrap();

        let hits = search_all(&st, "react", 10).unwrap();
        assert_eq!(hits.len(), 2);
        // Both are found by FTS; the exact-text entry should rank first
        // due to the exactness boost, regardless of storage/load order.
        match &hits[0] {
            SearchHit::Clipboard(c) => assert_eq!(c.text_content, "react"),
            _ => panic!("expected clipboard hit first"),
        }
    }

    #[test]
    fn search_all_type_filter_excludes_files_entirely() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&file_at("C:/p/jwt_notes.rs", "jwt_notes.rs", 100, None)).unwrap();
        st.save(&clip_at("eyJ.eyJ.sig", "jwt", 100)).unwrap();

        let hits = search_all(&st, "type:jwt", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0], SearchHit::Clipboard(_)));
    }

    #[test]
    fn search_all_today_listing_without_text() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();
        st.save(&file_at("C:/p/recent.rs", "recent.rs", now, None)).unwrap();
        st.save(&file_at("C:/p/ancient.rs", "ancient.rs", now - 100 * 86_400, None)).unwrap();

        let hits = search_all(&st, "today", 10).unwrap();
        assert_eq!(hits.len(), 1, "only the file modified within the last 24h should be listed");
    }

    #[test]
    fn search_all_detected_type_only_no_text_no_since() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&clip_at("eyJ.eyJ.sig", "jwt", 100)).unwrap();
        st.save(&clip_at("SELECT 1", "sql", 100)).unwrap();

        let hits = search_all(&st, "type:jwt", 10).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0] {
            SearchHit::Clipboard(c) => assert_eq!(c.detected_type, "jwt"),
            _ => panic!("expected clipboard"),
        }
    }

    #[test]
    fn search_all_empty_query_no_filters_returns_nothing() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&file_at("C:/p/a.rs", "a.rs", 100, None)).unwrap();
        assert!(search_all(&st, "", 10).unwrap().is_empty());
        assert!(search_all(&st, "   ", 10).unwrap().is_empty());
    }

    #[test]
    fn search_all_dedup_does_not_collide_file_and_clipboard_sharing_a_local_id() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        // Both of these are the first row in their own table, so both get
        // local_id == 1 - File(1) and Clipboard(1) must not be treated as
        // the same object.
        let file_id = st.save(&file_at("C:/p/shared.rs", "shared.rs", 100, None)).unwrap();
        let clip_id = st.save(&clip_at("shared note", "text", 100)).unwrap();
        assert_eq!(file_id.local_id, clip_id.local_id, "test setup: both must actually share a local_id");

        let hits = search_all(&st, "shared", 10).unwrap();
        assert_eq!(hits.len(), 2, "File(1) and Clipboard(1) must both survive dedup as distinct objects");
    }

    #[test]
    fn search_all_limit_is_applied_after_ranking_not_before() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        for i in 0..5 {
            st.save(&clip_at(&format!("needle entry number {i}"), "text", 100 + i)).unwrap();
        }
        let hits = search_all(&st, "needle", 2).unwrap();
        assert_eq!(hits.len(), 2, "limit must cap the final ranked list, not the raw candidate pool");
    }

    #[test]
    fn search_all_result_shape_matches_frontend_expectations() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let project_id = st.get_or_create_project("demo").unwrap();
        st.save(&file_at("C:/p/frontend_probe.rs", "frontend_probe.rs", 100, Some(project_id))).unwrap();
        st.save(&clip_at("frontend_probe clip", "text", 100)).unwrap();

        let hits = search_all(&st, "frontend_probe", 10).unwrap();
        for hit in hits {
            match hit {
                SearchHit::File(f) => {
                    assert_eq!(f.filename, "frontend_probe.rs");
                    assert!(!f.path.is_empty());
                }
                SearchHit::Clipboard(c) => {
                    assert_eq!(c.text_content, "frontend_probe clip");
                    assert_eq!(c.detected_type, "text");
                    assert!(!c.pinned);
                }
            }
        }
    }

    // ---- The standalone `search()` smoke-test command ----

    #[test]
    fn search_plain_files_only_fts_still_works() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&clip_at("hydration failed", "text", 100)).unwrap();
        let id = st.save(&file_at("C:/p/hydration_notes.rs", "hydration_notes.rs", 100, None)).unwrap();

        let results = search(&st, "hydration", None, 10).unwrap();
        assert_eq!(results.len(), 1, "the plain `search` command is files-only, unlike search_all");
        assert_eq!(results[0].id, id.local_id);
    }

    #[test]
    fn search_plain_since_only_listing_without_text() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();
        let id = st.save(&file_at("C:/p/recent.rs", "recent.rs", now, None)).unwrap();
        st.save(&file_at("C:/p/old.rs", "old.rs", now - 1000, None)).unwrap();

        let results = search(&st, "", Some(now - 10), 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id.local_id);
    }

    #[test]
    fn search_plain_empty_query_and_no_since_is_empty() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&file_at("C:/p/a.rs", "a.rs", 100, None)).unwrap();
        assert!(search(&st, "", None, 10).unwrap().is_empty());
    }
}
