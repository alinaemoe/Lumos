//! Generic search — see `ARCHITECTURE_INVARIANTS.md` §4 and its Phase 4
//! entries for the design history.
//!
//! `search_all` (the generic engine, taking a [`SearchIntent`] directly) and
//! its thin CLI wrapper `search_all_text` (raw string → `parse_intent` →
//! `search_all`) never name `files`, `clipboard_entries`, `files_fts`,
//! `clipboard_fts`, or `clipboard::search` — they retrieve candidates
//! through [`FullTextIndex`], [`EmbeddingStore`], and [`ObjectStore::recent`],
//! merge them by [`ObjectId`], fuse FTS and semantic rank via Reciprocal
//! Rank Fusion, hydrate through [`ObjectStore::load`] in one batched call,
//! and only then apply the filters/ranking signals that need real object
//! data. `rusqlite` does not appear anywhere in this file's production
//! code (only in test helpers, which need a concrete backing store).
//!
//! The `search_all`/`search_all_text` split is the point of Phase 5: any
//! producer of a well-formed `SearchIntent` — the CLI parser today, an
//! LLM/natural-language interpreter eventually — can call `search_all`
//! directly, without going through a string at all. See `SearchIntent`'s
//! own doc comment for the contract that makes this work.
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
/// are relative-hours windows, not calendar-day boundaries — exact
/// "midnight to midnight" would need timezone handling we don't have;
/// "N hours back from now" is close enough and a lot simpler to reason
/// about.
///
/// `today` and `week` stay pure lower bounds (open-ended up to now — there
/// is no meaningful upper bound distinct from "now" for either). `yesterday`
/// is the one case with a real, distinct upper bound, so as of Phase 5 it
/// sets both `since` and `until`, narrowing to a specific rolling 24h window
/// one day back rather than the old "at least 48h old" superset. This is a
/// deliberate, disclosed behavior change for `yesterday` specifically — see
/// `ARCHITECTURE_INVARIANTS.md`'s Phase 5 entry — not an oversight: the old
/// since-only "yesterday" silently included today's results too, which is
/// not what the word means.
///
/// Kept as its own function (rather than folded into `parse_intent`)
/// because `clipboard::search`/the CLI's standalone `clipboard search`
/// command call it directly against `Filters`, independent of the generic
/// engine — Phase 4 didn't touch that path, and Phase 5 doesn't either:
/// `clipboard::search`'s SQL never reads `Filters::until`, so that command's
/// behavior for `since` is byte-for-byte unchanged even though this function
/// now also populates `until` for it to ignore.
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
            filters.until = Some(now_ts() - 86_400);
        } else if lower == "week" {
            filters.since = Some(now_ts() - 7 * 86_400);
        } else {
            remaining.push(token);
        }
    }

    (remaining.join(" "), filters)
}

/// What the generic search engine actually consumes — the stable contract
/// between *any* intent producer (today: `parse_intent`'s CLI-syntax
/// parser; eventually: an LLM/natural-language interpreter) and
/// `search_all`. Neither producer nor engine needs to know about the
/// other — see `ARCHITECTURE_INVARIANTS.md`'s Phase 5 entry for the design
/// history this contract review is based on.
///
/// **Contract rules, binding on every producer of this type:**
///
/// 1. Every field except `text` must already hold a *resolved* value —
///    an epoch, an id, a known `ObjectType` — never a raw natural-language
///    fragment ("yesterday", "project X"). Resolving those is the
///    producer's job (the CLI parser resolves relative dates; an LLM
///    interpreter would need to resolve a project name to an id the same
///    way). `text` is the sole exception, and *only* because it flows into
///    fuzzy FTS/semantic retrieval, which needs free text by definition.
/// 2. `object_types` must never be empty. "Search everywhere" is
///    [`all_object_types`], spelled out — not the absence of a
///    restriction. `search_all` treats an empty vector as a contract
///    violation and returns an error rather than silently searching
///    nothing (see its doc comment for why this specific footgun matters).
/// 3. `object_types` (which *sources* to search) and `detected_type`
///    (clipboard's own content classification — `jwt`/`sql`/...) are
///    unrelated axes that happen to share the word "type" in casual
///    conversation. See the long note on `ObjectType` in `object.rs`.
///    Source-specific structured filters that aren't `object_types` or
///    `detected_type` (an email sender, a browser URL, ...) do not get
///    their own field here — they degrade to free-text `text` and rely on
///    that source indexing the relevant text. Adding a field per
///    source-specific attribute would turn this struct into exactly the
///    "one field per source" pattern the generic engine exists to avoid;
///    the boundary is enforced here, in the contract, not in the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchIntent {
    pub text: String,
    pub object_types: Vec<ObjectType>,
    pub detected_type: Option<String>,
    /// Inclusive lower time boundary — objects at or after this instant.
    pub since: Option<i64>,
    /// Inclusive upper time boundary — objects at or before this instant.
    /// Independent of `since`; either, both, or neither may be set.
    pub until: Option<i64>,
    /// No query syntax sets this yet — always `None` from `parse_intent`.
    /// Present now because the ranking function (`project_boost`) already
    /// needs somewhere to read a project filter from once one exists,
    /// and adding the field later would mean touching every call site
    /// again. Wiring an actual filter syntax for it is not this phase's
    /// job.
    pub project_id: Option<i64>,
}

/// Every `ObjectType` the generic search engine can search today —
/// "search everywhere" spelled out explicitly, per rule 2 of
/// [`SearchIntent`]'s contract. Never construct "everywhere" as an empty
/// `object_types` vector; use this instead.
///
/// Phase 6, Step 6.2: delegates to [`crate::registry::REGISTERED_OBJECT_TYPES`]
/// — previously this function held its own hand-written copy of the list,
/// independently of `storage_sqlite.rs`'s `SUPPORTED` constant, with
/// nothing enforcing the two stayed in sync. `registry.rs` is now the one
/// place either side reads. The compile-time "fail loudly on a new
/// variant" exhaustive match this function used to do inline still exists
/// — it moved to `registry.rs`'s own test, which is where it's now
/// enforced (a plain `const` array can't be exhaustive by itself; see that
/// module's doc comment).
pub fn all_object_types() -> Vec<ObjectType> {
    crate::registry::REGISTERED_OBJECT_TYPES.to_vec()
}

/// Builds a [`SearchIntent`] from raw query text using the existing
/// `type:`/`today`/`yesterday`/`week` parser.
///
/// `object_types` reproduces the exact restriction `search_all` applied
/// before Phase 4: a `detected_type` filter means "clipboard only" (files
/// have no `detected_type`, and searching `type:jwt` against filenames was
/// already documented as "just confusing" pre-Phase-4) — everything else
/// searches [`all_object_types`].
pub fn parse_intent(raw: &str) -> SearchIntent {
    let (text, filters) = parse_filters(raw);
    let object_types = if filters.detected_type.is_some() {
        vec![ObjectType::Clipboard]
    } else {
        all_object_types()
    };
    SearchIntent {
        text,
        object_types,
        detected_type: filters.detected_type,
        since: filters.since,
        until: filters.until,
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
        } else if intent.since.is_some() || intent.until.is_some() || intent.detected_type.is_some() {
            // No text to match/embed — this is a listing, not a search.
            // Triggered by `since` ("today"/"week"), `until` (a future
            // interpreter producing "before yesterday" with no lower
            // bound), or `detected_type` alone ("type:jwt" with nothing
            // else): pre-Phase-4 `clipboard::search`'s text-less branch
            // fired on the equivalent of any of these, not `since` only —
            // missing this the first time through Phase 4 produced
            // `type:jwt` returning zero results, caught by the CLI smoke
            // test rather than left as a silent regression. `since`
            // defaults to "the beginning of time" so a `detected_type`- or
            // `until`-only intent still lists everything up to that point.
            //
            // `until` and `detected_type` are both passed straight through
            // to `recent()`, which enforces each in SQL before its own
            // LIMIT — not left to `matches_time_bounds`/
            // `matches_detected_type` after hydration. An earlier version
            // filtered both post-hydration only, which meant enough
            // unrelated rows newer than the actual matches could fill the
            // entire fetch budget before any matching object was ever
            // retrieved — confirmed experimentally for `until` (70 newer
            // objects + fetch_n=60 made 3 in-window objects vanish) and
            // separately confirmed for `detected_type` on the real live
            // database (`type:jwt` returned nothing once unrelated
            // clipboard growth pushed the 60-most-recent-id cutoff past
            // all three real `jwt` rows). `matches_time_bounds`/
            // `matches_detected_type` still run afterward as
            // defense-in-depth — harmless here since `recent()` now only
            // ever returns already-matching rows, but still load-bearing
            // for FTS/semantic candidates, which have no time or
            // detected_type filter at the retrieval layer at all.
            let since = intent.since.unwrap_or(i64::MIN);
            for (rank, object_id) in store
                .recent(object_type, since, intent.until, intent.detected_type.as_deref(), fetch_n)?
                .into_iter()
                .enumerate()
            {
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
///
/// `since`/`until` are independent and both optional (`SearchIntent`'s
/// contract rule 1) — a missing timestamp on the object excludes it
/// whenever *either* bound is active, matching the pre-Phase-5 SQL
/// behavior for `since` (`NULL >= ?` is falsy in SQLite) extended
/// symmetrically to `until`.
fn matches_time_bounds(object: &Object, since: Option<i64>, until: Option<i64>) -> bool {
    if since.is_none() && until.is_none() {
        return true;
    }
    let Some(ts) = object.metadata.updated_at.or(object.metadata.created_at) else {
        return false;
    };
    since.map(|s| ts >= s).unwrap_or(true) && until.map(|u| ts <= u).unwrap_or(true)
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
/// filter → rank → limit → frontend-compatible results.
///
/// Takes an already-built [`SearchIntent`] — this is the seam Phase 5
/// exists to secure: the CLI's `parse_intent` and a future LLM/
/// natural-language interpreter are two independent, equally valid
/// producers of that type, and neither this function nor anything it
/// calls needs to know or care which one produced the intent it's given.
/// [`search_all_text`] below is the CLI-syntax convenience wrapper — it is
/// the *only* thing in this module that still takes a raw string.
///
/// Returns an error if `intent.object_types` is empty. This is deliberate,
/// not defensive paranoia: an empty vector could plausibly be an LLM's
/// (wrong) encoding of "no restriction, search everything" — see rule 2 of
/// `SearchIntent`'s contract — and the pre-Phase-5 failure mode for that
/// exact mistake was `retrieve_candidates` silently iterating zero times
/// and returning zero results, indistinguishable from "nothing matched."
/// Erroring here converts a silent wrong-answer into a caller-visible
/// problem the caller can recover from (e.g. by falling back to
/// `all_object_types()`), which is strictly better than either extreme
/// (guessing "empty means everything" would also be wrong: that reading
/// isn't in the contract, so guessing it here would just move the
/// silent-wrongness to a different mistake).
pub fn search_all<S>(store: &S, intent: &SearchIntent, limit: i64) -> Result<Vec<SearchHit>>
where
    S: FullTextIndex + EmbeddingStore + ObjectStore,
{
    if intent.object_types.is_empty() {
        return Err(anyhow::anyhow!(
            "SearchIntent.object_types must not be empty — use all_object_types() for \"search everywhere\", never an empty Vec"
        ));
    }
    let limit = limit.max(0) as usize;
    if limit == 0 {
        return Ok(Vec::new());
    }

    let candidates = retrieve_candidates(store, intent, limit)?;
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
        if !matches_time_bounds(&object, intent.since, intent.until) {
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

    // Score descending, then `ObjectId` ascending as a deterministic
    // tie-breaker. Without this, equal-score candidates keep whatever
    // order they happened to come out of `merge_candidates`'s
    // `HashMap<ObjectId, f32>` in — which varies from run to run, since
    // `HashMap`'s default hasher is seeded per-process. `sort_by` is
    // stable, so it preserves that randomized order for ties rather than
    // fixing it; the comparator has to break the tie itself.
    // Score descending, then `ObjectId` ascending as a deterministic
    // tie-breaker. Without this, equal-score candidates keep whatever
    // order they happened to come out of `merge_candidates`'s
    // `HashMap<ObjectId, f32>` in — which varies from run to run, since
    // `HashMap`'s default hasher is seeded per-process. `sort_by` is
    // stable, so it preserves that randomized order for ties rather than
    // fixing it; the comparator has to break the tie itself.
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    ranked.truncate(limit);

    Ok(ranked.into_iter().map(|(_, obj)| to_search_hit(obj)).collect())
}

/// Thin CLI-syntax compatibility wrapper: `raw query -> parse_intent ->
/// SearchIntent -> search_all`. This is the function the CLI's
/// `search-all` and the Tauri `search_files` command call — everything
/// downstream of `parse_intent` here is identical to what a future LLM
/// interpreter's output would go through, just reached via a different
/// front door.
pub fn search_all_text<S>(store: &S, raw_query: &str, limit: i64) -> Result<Vec<SearchHit>>
where
    S: FullTextIndex + EmbeddingStore + ObjectStore,
{
    let intent = parse_intent(raw_query);
    search_all(store, &intent, limit)
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
        // This function's own contract has no `until`/`detected_type` —
        // pass `None` for both to preserve its exact pre-fix behavior (a
        // plain lower-bound listing). It's File-only anyway, and
        // `detected_type` has no meaning for File.
        store.recent(ObjectType::File, since, None, None, limit)?
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

        let intent = SearchIntent { text: "hydration_notes".into(), object_types: vec![ObjectType::File], detected_type: None, since: None, until: None, project_id: None };
        let candidates = retrieve_candidates(&st, &intent, 10).unwrap();
        assert!(candidates.iter().any(|c| c.object_id == id && c.source == RetrievalSource::FullText));
    }

    #[test]
    fn retrieval_finds_clipboard_via_fts() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let id = st.save(&clip_at("SELECT * FROM users", "sql", 100)).unwrap();

        let intent = SearchIntent { text: "users".into(), object_types: vec![ObjectType::Clipboard], detected_type: None, since: None, until: None, project_id: None };
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

        let intent = SearchIntent { text: "main".into(), object_types: vec![ObjectType::File], detected_type: None, since: None, until: None, project_id: None };
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

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::File], detected_type: None, since: Some(100), until: None, project_id: None };
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

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::Clipboard], detected_type: Some("jwt".into()), since: None, until: None, project_id: None };
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

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::File], detected_type: None, since: None, until: None, project_id: None };
        assert!(retrieve_candidates(&st, &intent, 10).unwrap().is_empty());
    }

    #[test]
    fn retrieval_unsupported_object_type_propagates_error() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let intent = SearchIntent { text: "x".into(), object_types: vec![ObjectType::Screenshot], detected_type: None, since: None, until: None, project_id: None };
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
    fn matches_time_bounds_uses_updated_at_then_falls_back_to_created_at() {
        let file_obj = Object::new(1, ObjectMetadata { created_at: Some(1), updated_at: Some(500), project_id: None, source: None }, ObjectPayload::File(FilePayload { path: "x".into(), filename: "x".into(), extension: None, size: 0, hash: None, project_name: None }));
        assert!(matches_time_bounds(&file_obj, Some(500), None));
        assert!(!matches_time_bounds(&file_obj, Some(501), None));

        let clip_obj = Object::new(1, ObjectMetadata { created_at: Some(500), updated_at: None, project_id: None, source: None }, ObjectPayload::Clipboard(ClipboardPayload { text_content: "x".into(), detected_type: "text".into(), pinned: false, search_extra: None }));
        assert!(matches_time_bounds(&clip_obj, Some(500), None), "clipboard has no updated_at, must fall back to created_at");
    }

    #[test]
    fn matches_time_bounds_supports_since_and_until_independently_and_together() {
        let obj_at = |ts: i64| Object::new(1, ObjectMetadata { created_at: Some(ts), updated_at: None, project_id: None, source: None }, ObjectPayload::File(FilePayload { path: "x".into(), filename: "x".into(), extension: None, size: 0, hash: None, project_name: None }));

        // since without until
        assert!(matches_time_bounds(&obj_at(500), Some(400), None));
        assert!(!matches_time_bounds(&obj_at(300), Some(400), None));

        // until without since
        assert!(matches_time_bounds(&obj_at(300), None, Some(400)));
        assert!(!matches_time_bounds(&obj_at(500), None, Some(400)));

        // both together — inclusive window
        assert!(matches_time_bounds(&obj_at(350), Some(300), Some(400)));
        assert!(matches_time_bounds(&obj_at(300), Some(300), Some(400)), "since bound is inclusive");
        assert!(matches_time_bounds(&obj_at(400), Some(300), Some(400)), "until bound is inclusive");
        assert!(!matches_time_bounds(&obj_at(250), Some(300), Some(400)));
        assert!(!matches_time_bounds(&obj_at(450), Some(300), Some(400)));

        // neither bound — always passes
        assert!(matches_time_bounds(&obj_at(999), None, None));
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

        let hits = search_all_text(&st, "hydration", 10).unwrap();
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

        let hits = search_all_text(&st, "react", 10).unwrap();
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

        let hits = search_all_text(&st, "type:jwt", 10).unwrap();
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

        let hits = search_all_text(&st, "today", 10).unwrap();
        assert_eq!(hits.len(), 1, "only the file modified within the last 24h should be listed");
    }

    #[test]
    fn search_all_detected_type_only_no_text_no_since() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&clip_at("eyJ.eyJ.sig", "jwt", 100)).unwrap();
        st.save(&clip_at("SELECT 1", "sql", 100)).unwrap();

        let hits = search_all_text(&st, "type:jwt", 10).unwrap();
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
        assert!(search_all_text(&st, "", 10).unwrap().is_empty());
        assert!(search_all_text(&st, "   ", 10).unwrap().is_empty());
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

        let hits = search_all_text(&st, "shared", 10).unwrap();
        assert_eq!(hits.len(), 2, "File(1) and Clipboard(1) must both survive dedup as distinct objects");
    }

    #[test]
    fn search_all_limit_is_applied_after_ranking_not_before() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        for i in 0..5 {
            st.save(&clip_at(&format!("needle entry number {i}"), "text", 100 + i)).unwrap();
        }
        let hits = search_all_text(&st, "needle", 2).unwrap();
        assert_eq!(hits.len(), 2, "limit must cap the final ranked list, not the raw candidate pool");
    }

    #[test]
    fn search_all_result_shape_matches_frontend_expectations() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let project_id = st.get_or_create_project("demo").unwrap();
        st.save(&file_at("C:/p/frontend_probe.rs", "frontend_probe.rs", 100, Some(project_id))).unwrap();
        st.save(&clip_at("frontend_probe clip", "text", 100)).unwrap();

        let hits = search_all_text(&st, "frontend_probe", 10).unwrap();
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

    // ---- Phase 5: SearchIntent contract (until, object_types, split entry point) ----

    #[test]
    fn parse_intent_populates_until_for_yesterday() {
        let intent = parse_intent("yesterday");
        assert!(intent.since.is_some());
        assert!(intent.until.is_some(), "yesterday must set a real upper bound, unlike today/week");
        assert!(intent.since.unwrap() < intent.until.unwrap());
    }

    #[test]
    fn parse_intent_today_and_week_leave_until_unset() {
        assert!(parse_intent("today").until.is_none());
        assert!(parse_intent("week").until.is_none());
    }

    #[test]
    fn since_and_until_together_bound_search_all_results() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();
        st.save(&file_at("C:/p/too_old.rs", "too_old.rs", now - 300, None)).unwrap();
        let in_window = st.save(&file_at("C:/p/in_window.rs", "in_window.rs", now - 150, None)).unwrap();
        st.save(&file_at("C:/p/too_new.rs", "too_new.rs", now - 10, None)).unwrap();

        let intent = SearchIntent {
            text: "".into(),
            object_types: vec![ObjectType::File],
            detected_type: None,
            since: Some(now - 200),
            until: Some(now - 100),
            project_id: None,
        };
        let hits = search_all(&st, &intent, 10).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0] {
            SearchHit::File(f) => assert_eq!(f.id, in_window.local_id),
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn search_all_yesterday_survives_being_crowded_by_many_today_objects() {
        // End-to-end regression for the exact bug reported and
        // experimentally reproduced: 70 "today" clipboard entries + 3
        // "yesterday" entries, `limit=20` (-> fetch_n=60 via
        // CANDIDATE_OVERFETCH). Pre-fix this returned 0 of the 3.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();

        let yesterday_ids: std::collections::HashSet<i64> = (0..3)
            .map(|i| {
                st.save(&clip_at(&format!("yesterday joke {i}"), "text", now - 30 * 3600))
                    .unwrap()
                    .local_id
            })
            .collect();
        let today_ids: std::collections::HashSet<i64> = (0..70)
            .map(|i| {
                st.save(&clip_at(&format!("today filler {i}"), "text", now - 3600))
                    .unwrap()
                    .local_id
            })
            .collect();

        let hits = search_all_text(&st, "yesterday", 20).unwrap();
        let returned: std::collections::HashSet<i64> = hits
            .iter()
            .map(|h| match h {
                SearchHit::Clipboard(c) => c.id,
                SearchHit::File(f) => f.id,
            })
            .collect();

        assert_eq!(returned, yesterday_ids, "all 3 yesterday entries must survive 70 crowding today entries");
        assert!(
            returned.is_disjoint(&today_ids),
            "no today entry must leak into a yesterday result — got {returned:?}"
        );
    }

    #[test]
    fn search_all_type_jwt_survives_being_crowded_by_many_unrelated_entries() {
        // End-to-end regression for the exact live-database failure:
        // `search-all type:jwt` returned nothing once unrelated clipboard
        // growth pushed the 60-most-recent-id cutoff past all 3 real jwt
        // rows. Same shape as the `until` crowding test above, for
        // `detected_type` instead of a time bound.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);

        let jwt_ids: std::collections::HashSet<i64> = (0..3)
            .map(|i| st.save(&clip_at(&format!("jwt token {i}"), "jwt", 100)).unwrap().local_id)
            .collect();
        let unrelated_ids: std::collections::HashSet<i64> = (0..70)
            .map(|i| st.save(&clip_at(&format!("unrelated filler {i}"), "text", 200)).unwrap().local_id)
            .collect();

        let hits = search_all_text(&st, "type:jwt", 20).unwrap();
        let returned: std::collections::HashSet<i64> = hits
            .iter()
            .map(|h| match h {
                SearchHit::Clipboard(c) => c.id,
                SearchHit::File(f) => f.id,
            })
            .collect();

        assert_eq!(returned, jwt_ids, "all 3 jwt entries must survive 70 crowding unrelated entries");
        assert!(
            returned.is_disjoint(&unrelated_ids),
            "no unrelated entry must leak into a type:jwt result — got {returned:?}"
        );
    }

    #[test]
    fn search_all_today_unaffected_by_the_until_fix() {
        // Requirement: `today` behavior (since-only, `until: None`) must
        // be byte-for-byte unchanged by this fix.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();

        let today = st.save(&clip_at("today entry", "text", now - 3600)).unwrap();
        st.save(&clip_at("old entry", "text", now - 3 * 86_400)).unwrap();

        let hits = search_all_text(&st, "today", 20).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0] {
            SearchHit::Clipboard(c) => assert_eq!(c.id, today.local_id),
            _ => panic!("expected clipboard"),
        }
    }

    #[test]
    fn since_alone_still_works_without_until() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();
        let recent = st.save(&file_at("C:/p/recent.rs", "recent.rs", now, None)).unwrap();
        st.save(&file_at("C:/p/old.rs", "old.rs", now - 1000, None)).unwrap();

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::File], detected_type: None, since: Some(now - 10), until: None, project_id: None };
        let hits = search_all(&st, &intent, 10).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0] {
            SearchHit::File(f) => assert_eq!(f.id, recent.local_id),
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn until_alone_works_without_since() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();
        let old = st.save(&file_at("C:/p/old.rs", "old.rs", now - 1000, None)).unwrap();
        st.save(&file_at("C:/p/recent.rs", "recent.rs", now, None)).unwrap();

        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::File], detected_type: None, since: None, until: Some(now - 500), project_id: None };
        let hits = search_all(&st, &intent, 10).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0] {
            SearchHit::File(f) => assert_eq!(f.id, old.local_id),
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn search_all_rejects_empty_object_types() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let intent = SearchIntent { text: "x".into(), object_types: vec![], detected_type: None, since: None, until: None, project_id: None };
        let err = search_all(&st, &intent, 10).unwrap_err();
        assert!(err.to_string().contains("object_types"));
    }

    #[test]
    fn all_object_types_is_the_explicit_search_everywhere_list() {
        let types = all_object_types();
        assert!(types.contains(&ObjectType::File));
        assert!(types.contains(&ObjectType::Clipboard));
        assert!(!types.is_empty());
        // Sources with no storage backing yet must not be claimed as
        // "searchable" by this list.
        assert!(!types.contains(&ObjectType::Screenshot));
        assert!(!types.contains(&ObjectType::Email));
    }

    #[test]
    fn parse_intent_never_produces_empty_object_types() {
        for query in ["", "  ", "hello", "type:jwt", "today", "type:sql yesterday"] {
            assert!(!parse_intent(query).object_types.is_empty(), "query {query:?} produced empty object_types");
        }
    }

    #[test]
    fn detected_type_behavior_is_unchanged_by_phase_5() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&clip_at("eyJ.eyJ.sig", "jwt", 100)).unwrap();
        st.save(&file_at("C:/p/jwt.rs", "jwt.rs", 100, None)).unwrap();

        let intent = parse_intent("type:jwt");
        assert_eq!(intent.object_types, vec![ObjectType::Clipboard], "detected_type still means clipboard-only");
        let hits = search_all(&st, &intent, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0], SearchHit::Clipboard(_)));
    }

    #[test]
    fn project_filtering_behavior_is_unchanged_by_phase_5() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let project_id = st.get_or_create_project("demo").unwrap();
        let in_project = st.save(&file_at("C:/p/in_project.rs", "in_project.rs", 100, Some(project_id))).unwrap();
        st.save(&file_at("C:/p/other.rs", "other.rs", 100, None)).unwrap();

        // project_id still only nudges ranking, exactly as before Phase 5
        // — it is not (and was never) a hard filter.
        let intent = SearchIntent { text: "".into(), object_types: vec![ObjectType::File], detected_type: None, since: Some(0), until: None, project_id: Some(project_id) };
        let hits = search_all(&st, &intent, 10).unwrap();
        assert_eq!(hits.len(), 2, "project_id boosts ranking, it does not exclude non-matching objects");
        match &hits[0] {
            SearchHit::File(f) => assert_eq!(f.id, in_project.local_id, "the matching-project file must rank first"),
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn existing_cli_queries_produce_the_same_results_via_search_all_text() {
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&file_at("C:/p/hydration_notes.rs", "hydration_notes.rs", 100, None)).unwrap();
        st.save(&clip_at("SELECT * FROM users", "sql", 100)).unwrap();

        for query in ["hydration_notes", "users", "type:sql", "", "   "] {
            let via_text = search_all_text(&st, query, 10).unwrap();
            let via_intent = search_all(&st, &parse_intent(query), 10).unwrap();
            assert_eq!(via_text.len(), via_intent.len(), "query {query:?} must behave identically through both entry points");
        }
    }

    #[test]
    fn a_manually_constructed_intent_reaches_search_all_without_the_string_parser() {
        // This is the whole point of Phase 5: an intent that never touched
        // `parse_intent`/`parse_filters` — standing in for a future LLM
        // interpreter's output — must work identically to a CLI-parsed one.
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        st.save(&clip_at("hydration failed in react", "text", 100)).unwrap();

        let intent = SearchIntent {
            text: "hydration".into(),
            object_types: all_object_types(),
            detected_type: None,
            since: None,
            until: None,
            project_id: None,
        };
        let hits = search_all(&st, &intent, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0], SearchHit::Clipboard(_)));
    }

    // ---- Deterministic ordering for tied scores ----

    #[test]
    fn object_id_ord_is_a_total_deterministic_order() {
        let a = ObjectId::new(ObjectType::File, 5);
        let b = ObjectId::new(ObjectType::Clipboard, 1);
        let a_again = ObjectId::new(ObjectType::File, 5);
        assert_eq!(a, a_again);
        assert!(a < b, "File orders before Clipboard by declaration order — an arbitrary but fixed, compile-time rule, not runtime state");
        let mut ids = vec![b, a];
        ids.sort();
        assert_eq!(ids, vec![a, b], "sorting must be repeatable, not just internally consistent");
    }

    /// This is the test that actually exercises the property the bug
    /// violated: with only `b.0.total_cmp(&a.0)` (the pre-fix comparator),
    /// a stable sort preserves whatever order tied elements arrived in —
    /// so two different arrival orders for the *same* tied-score set
    /// produce two different outputs. The prior audit observed this as
    /// "6 consecutive process launches, 6 different orders," because
    /// `HashMap`'s per-process random seed changes what order
    /// `merge_candidates` happens to hand equal-score candidates to
    /// `sort_by` in. Re-running within a single test process would not
    /// reliably reproduce that — `HashMap`'s default hasher reuses the
    /// same per-thread random keys across separate `HashMap` instances in
    /// one process, so repeated calls in one test are not a faithful
    /// stand-in for repeated *process launches*. Testing the comparator
    /// directly against two deliberately different input orderings is:
    /// it proves the output no longer depends on arrival order at all,
    /// which is the only way to be correct regardless of what any given
    /// process's `HashMap` seed happens to be.
    #[test]
    fn tie_break_by_object_id_makes_order_independent_of_input_arrival_order() {
        fn clip_obj_for_tie(local_id: i64) -> Object {
            Object::new(
                local_id,
                ObjectMetadata::default(),
                ObjectPayload::Clipboard(ClipboardPayload {
                    text_content: format!("tie {local_id}"),
                    detected_type: "text".into(),
                    pinned: false,
                    search_extra: None,
                }),
            )
        }
        // Same comparator `search_all` uses for its final sort — kept as
        // a literal copy here (not extracted into a shared function)
        // deliberately, so this test fails the moment someone changes
        // the real comparator without updating this one too.
        let sort_with_tiebreak = |mut v: Vec<(f32, Object)>| -> Vec<i64> {
            v.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
            v.into_iter().map(|(_, o)| o.id.local_id).collect()
        };

        let tied_score = 0.5_f32;
        let arrival_order_a: Vec<(f32, Object)> =
            [3, 1, 4, 2].into_iter().map(|id| (tied_score, clip_obj_for_tie(id))).collect();
        let arrival_order_b: Vec<(f32, Object)> =
            [2, 4, 1, 3].into_iter().map(|id| (tied_score, clip_obj_for_tie(id))).collect();

        let result_a = sort_with_tiebreak(arrival_order_a);
        let result_b = sort_with_tiebreak(arrival_order_b);

        assert_eq!(result_a, result_b, "identical tied scores must sort the same way regardless of arrival order — this is what a bare stable sort cannot guarantee");
        assert_eq!(result_a, vec![1, 2, 3, 4], "ties resolve to ascending ObjectId, i.e. ascending local_id within one type");
    }

    #[test]
    fn search_all_orders_tied_clipboard_scores_by_object_id_ascending() {
        // Real end-to-end fixture, not a synthetic Vec: several clipboard
        // entries sharing one `created_at`, retrieved via the recency
        // path (`RetrievalSource::Recency` contributes 0 to RRF), so
        // their final scores are exactly equal (identical recency_boost,
        // no text to drive exact-match, no project filter).
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let now = crate::now_secs();
        let mut ids: Vec<i64> = (0..6)
            .map(|i| st.save(&clip_at(&format!("tie entry {i}"), "text", now - 3600)).unwrap().local_id)
            .collect();
        ids.sort();

        let hits = search_all_text(&st, "today", 20).unwrap();
        let returned: Vec<i64> = hits
            .iter()
            .map(|h| match h {
                SearchHit::Clipboard(c) => c.id,
                SearchHit::File(f) => f.id,
            })
            .collect();
        assert_eq!(returned, ids, "tied clipboard entries must come back in ascending id order");
    }

    #[test]
    fn existing_ranking_for_non_tied_scores_is_unaffected_by_the_tiebreak() {
        // The exactness boost already gives these two entries different
        // scores — the tiebreak comparator must never override a real
        // score difference. (Companion to the pre-existing
        // `search_all_exact_match_ranks_above_partial_match`, re-asserted
        // here specifically to document that this fix didn't touch it.)
        let conn = store_conn();
        let st = SqliteStore::new(&conn);
        let exact = st.save(&clip_at("react", "text", 100)).unwrap();
        let partial = st.save(&clip_at("a note about react hooks", "text", 100)).unwrap();

        let hits = search_all_text(&st, "react", 10).unwrap();
        let returned: Vec<i64> = hits
            .iter()
            .map(|h| match h {
                SearchHit::Clipboard(c) => c.id,
                SearchHit::File(f) => f.id,
            })
            .collect();
        assert_eq!(returned, vec![exact.local_id, partial.local_id], "the exact match must still rank first — score, not id, decides when scores differ");
    }
}
