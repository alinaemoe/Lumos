# Lumos — Architecture Invariants

**Status:** binding. This is not a design proposal and not a roadmap. It describes rules that every module — existing or future — must obey. A change that violates an invariant here is a bug, even if it compiles, passes tests, and ships a feature the user wants. If a real need conflicts with an invariant, the invariant is amended deliberately, in this document, with a stated reason — it is never silently worked around in a module.

**Audience:** anyone (including future-you, including an AI assistant) about to add a data source, touch Search, or wire a new UI surface into Lumos.

**Time horizon:** these rules should still be true five years from now, after Files, Clipboard, Screenshots, Browser, Sessions, Email, and things not yet imagined have all been added. If a rule only makes sense for the current two sources, it doesn't belong in this document — it belongs in a module's own README.

---

## 0. The one-sentence architecture

Lumos is a **pipeline that turns raw personal activity into searchable Objects**, plus a **generic search layer that has never heard of Files or Clipboard**, plus a **UI that has never heard of SQLite**. Every invariant below is a consequence of that one sentence.

---

## 1. The Object model

Every piece of data Lumos ever ingests — a file, a clipboard entry, a screenshot, a browser visit, an email, a session — is represented the same way:

```
Object
 ├─ identity     : global unique id + object_type (stable, never reused)
 ├─ metadata     : common fields every object has (created_at, source, project_id, ...)
 ├─ payload      : type-specific content (file path+hash, clipboard text, screenshot bytes ref, ...)
 └─ relations    : links to other Objects (a screenshot belongs to a session; a clipboard entry
                   was copied while a file was open; an email references a project)
```

**Invariants:**

- **1.1** — There is exactly one identity scheme for all objects, not one per source table. Any code that needs to refer to "a thing in Lumos" refers to it by `(object_type, object_id)` or an equivalent single global key — never by a source-specific primary key leaking into a cross-cutting layer (Search, ranking, dedup, relations).
- **1.2** — Metadata common to *all* objects (creation time, source module, owning project, tags) lives in one shared shape. A new object type does not get to invent its own name for "when was this created."
- **1.3** — Payload is source-specific and *only* the owning module is allowed to interpret it. Search, ranking, and UI-shell code must never branch on payload internals (`if it's a File, read .hash`) — see §4.
- **1.4** — Relations are first-class, not implicit. If a screenshot belongs to a session, that is a stored relation `(session_id, screenshot_id, relation_type)`, not an inferred join on timestamps at query time. This is what lets Sessions attach to Files/Clipboard/Screenshots without any of those modules knowing Sessions exist (see §7, plugin rule).
- **1.5** — An object's lifecycle is: `created → updated (0..n times) → deleted`. Deletion is a real state transition (the object and its embeddings/index entries are actually removed or tombstoned), never a silent orphan left in a downstream table. Every module that stores derived data keyed by `(object_type, object_id)` (embeddings, OCR text, thumbnails) is responsible for cleaning it up on delete — no exceptions, no "cron job will get it eventually."

---

## 2. The ingestion pipeline

Every data source — present or future — is ingested through the same shape of pipeline:

```
Watch → Parse → Normalize → Classify → Embed → Store → Index
```

| Stage | Responsibility | Must NOT do |
|---|---|---|
| **Watch** | Detect that something happened (file changed, clipboard changed, a new screenshot was taken, a browser tab closed). Produces a raw event. | Must not touch the database. Must not classify or interpret content. |
| **Parse** | Turn the raw event into a source-specific payload (read file bytes/stat, read clipboard buffer, decode a screenshot). | Must not decide project ownership or run classification. |
| **Normalize** | Shape the parsed payload into the common Object envelope (§1) — assign/confirm identity, fill common metadata. | Must not embed or classify yet. |
| **Classify** | Deterministic, rule-based typing (clipboard type detection, repo-type detection, mime type). No ML required — and per §8, none of this pipeline may *require* ML to produce a usable result. | Must not require a network call or an LLM. |
| **Embed** | Optional, best-effort. Produces a vector for semantic search. | Failure here must never fail the pipeline (§8). Must not be on the critical path of Watch/Parse/Normalize. |
| **Store** | Persist the Object (and its payload) to the datastore. | Must not know about Search ranking or UI. |
| **Index** | Update whatever the Search layer needs (FTS index, embedding table, future ANN index). | Must not duplicate logic that Store already did. |

**Invariants:**

- **2.1** — A new source (Browser, Screenshots, Email, Telegram, anything) is implemented by writing these seven stages for that source. It is never implemented by adding a special case inside an existing source's pipeline, and never by adding a new branch inside Search.
- **2.2** — Every stage takes the previous stage's output and nothing else. A `Classify` implementation must never reach back and re-read raw event state that `Parse`/`Normalize` already consumed — that's a sign the pipeline shape is being bypassed.
- **2.3** — `Watch` is the only stage allowed to depend on OS-specific APIs directly (`ReadDirectoryChangesW`, `WM_CLIPBOARDUPDATE`-equivalents, hooks). Every later stage works with plain Rust data, so it is testable without Windows.
- **2.4** — The pipeline is the *only* path into storage. There is no second, ad hoc way to insert an Object (e.g. a CLI command that writes directly to the `files` table bypassing Classify/Embed). If the CLI needs to insert data, it calls the same pipeline entry point the watcher does.

---

## 3. Ingestion is plugin-shaped

- **3.1** — A data source is a **plugin**: an implementation of the seven-stage pipeline plus a declaration of what Object type(s) it produces. Adding Browser or Email must not require editing Core, Search, or any existing source's code.
- **3.2** — Core owns the pipeline *shape* (the trait/interface each stage implements) and the registry of active sources. Core does not own — and must never contain — source-specific logic (no `if source == "browser"` anywhere in Core).
- **3.3** — A source plugin may be entirely absent (not compiled in, not registered, feature-flagged off) and the rest of Lumos must not notice beyond "that object type returns zero results." This is what makes "Screenshots doesn't exist yet" a valid, safe state rather than a special case to code around everywhere else.

---

## 4. Search is generic

Search must never know what a File or a Clipboard entry or a Screenshot *is*. It knows only:

```
SearchObject   — the generic, indexed shape every Object exposes to Search
                 (id, object_type, indexable_text, embedding_ref, metadata for filtering/ranking)
SearchIndex    — FTS + vector index over SearchObjects, source-agnostic
SearchProvider — the seam a source plugin implements to hand its Objects to Search
                 as SearchObjects
```

```
User Query
   ↓
Search Intent (parsed filters + free text — see §5)
   ↓
SearchIndex.query(intent)          ← talks only to SearchObject, never to File/Clipboard/etc.
   ↓
Merge (single candidate list, not per-source blocks)
   ↓
Ranking (one scoring function over the merged list — FTS score and vector score fused,
          e.g. RRF — never "FTS block first, semantic block second")
   ↓
Dedup (against the merged, ranked list — by object identity AND by near-duplicate content)
   ↓
UI
```

**Invariants:**

- **4.1** — Search Engine code contains zero references to `File`, `Clipboard`, `Screenshot`, or any other concrete source type. If Search needs to special-case a type, that is a sign the `SearchObject` abstraction is missing a field — fix the abstraction, not Search.
- **4.2** — A new `SearchProvider` is how a new source becomes searchable. Search does not grow a new code path per source; sources grow a new adapter into the existing Search.
- **4.3** — Merge, Ranking, and Dedup operate on one unified candidate list. Concatenating per-source result blocks in a fixed order is explicitly forbidden — it is not "ranking," it is an ordering accident, and it is the single largest search-quality defect this architecture can have.
- **4.4** — Ranking is one function that takes heterogeneous signals (FTS rank, cosine similarity, recency, pin state) and produces one comparable score per candidate. FTS and vector scores are never displayed/ordered as if they lived on the same scale without an explicit fusion step (RRF or normalized weighted sum) — see the code review's Phase 4 for the concrete mechanics.
- **4.5** — Dedup operates on the merged list, keyed by object identity (`object_type`, `object_id`) at minimum, and should account for near-duplicate content (not just identical ids) as sources multiply. Two different Search blocks silently deduping independently (as happens today) is a violation once Merge/Ranking are unified — dedup moves to *after* merge, not before.

---

## 5. The LLM boundary

- **5.1** — The LLM (when present, per the design doc's "AI Interpreter") **never** touches the filesystem, never opens a database connection, never issues SQL, never reads raw Object payloads directly.
- **5.2** — The LLM's only contract: `natural language query → structured Search Intent` (a fixed, versioned schema: object type filters, time range, project, free-text remainder). It fills a schema; it does not generate a query language of any kind.
- **5.3** — Everything downstream of Search Intent is deterministic code. If the LLM is unavailable, Search Intent parsing falls back to the deterministic rule-based parser (today's `type:`/`today`/`week` token extraction) — degraded, not broken. This is a direct instance of §8.
- **5.4** — The LLM is never on the ingestion path. Classification, embedding, and metadata resolution are deterministic/ML-small-model, not LLM, for every source, always — matching the design doc's existing stance and extending it as a hard rule, not a preference.

---

## 6. Module boundaries and dependency direction

Three layers, one allowed direction of dependency:

```
UI  →  Core (domain: pipeline, Search, Object model)  →  Infrastructure (SQLite, ONNX, Win32, FS)
```

- **6.1 — UI contains no business logic.** UI code renders state and translates user input into calls against Core's public surface (Tauri commands / CLI subcommands). Classification rules, ranking math, filter parsing, dedup logic — none of it lives in `app.js` or any future UI layer. If a UI file starts computing a score or deciding what counts as a duplicate, that logic has leaked and must move to Core.
- **6.2 — Core never depends on UI.** `lumos-core` must not import anything from `apps/lumos-app` or know that Tauri exists. Today this already holds structurally (core is a separate crate) — the rule is that it must keep holding as commands multiply.
- **6.3 — Infrastructure never leaks into the domain.** Domain code (the pipeline stages, the Object model, Search) speaks in domain terms (`Object`, `SearchIntent`, `EmbeddingVector`) — never in `rusqlite::Connection`, `ort::Session`, or `HWND`. Infrastructure concerns are reached through a narrow trait/interface the domain defines and infrastructure implements, not the other way around. Concretely: a pipeline stage function signature should not force every caller to hand it a raw SQLite connection type if a storage trait would do — this is aspirational relative to today's code (which does pass `&Connection` directly throughout `lumos-core`) and is flagged as a gap to close incrementally, not retrofitted in one pass.
- **6.4 — Allowed dependencies:**
  - UI → Core (via commands/CLI only, never by importing Core internals directly into rendering logic)
  - Core (pipeline, Object model, Search) → Infrastructure traits (storage, embedding, OS integration)
  - A source plugin (§3) → Core's pipeline trait + Infrastructure traits it needs (e.g. Win32 for Watch)
- **6.5 — Forbidden dependencies:**
  - Core → UI (in either direction of the crate graph, not just avoided by convention)
  - Search → any concrete source module (`search.rs` importing `clipboard::ClipboardResult` directly is exactly the pattern §4.1 forbids)
  - One source plugin → another source plugin's internals (Files must not import Clipboard's classifier, Screenshots must not query `clipboard_entries` directly even if it seems convenient — relations, per §1.4, are how sources reference each other)
  - Infrastructure → domain (a storage implementation must not contain business rules about what a valid Object looks like — that validation lives in the domain, storage just persists)

---

## 7. Extensibility is non-negotiable

- **7.1** — Adding a new Object source must touch: a new plugin module (implementing the seven pipeline stages + a `SearchProvider` adapter) and a registration line in Core's source registry. It must not require edits to `search.rs`'s core logic, to another source's module, or to UI rendering beyond adding a new result-kind branch in the display layer (rendering *is* allowed to know about kinds for icon/preview purposes — that is presentation, not business logic; see §6.1's distinction).
- **7.2** — A source plugin can be compiled out, feature-flagged off, or simply not yet written, and the rest of the system remains correct — it just returns fewer results. This must hold at every point in time, not just "eventually, once everything is built." (This is why today's Screenshots/Browser/Sessions absence is a valid state, not technical debt — see §3.3.)

---

## 8. Graceful degradation is a hard requirement, not a nicety

- **8.1** — Every optional subsystem (embedding model, OCR, LLM interpreter, CLIP, any future ML component) has a defined degraded mode, and the system is fully specified in that mode — not "probably still works," but explicitly designed to still work.
  - No embedding model available → FTS-only search, full functionality minus semantic recall.
  - No LLM available → deterministic filter parsing instead of natural-language intent parsing.
  - OCR unavailable → screenshots are searchable by whatever non-OCR metadata they have (time, project, manual tags), not unsearchable.
- **8.2** — A failure in an optional subsystem must never propagate as an error to the ingestion pipeline or to the user. `embeddings::store()` failing must not fail `index_file()`; this already holds today (best-effort, errors swallowed) and must hold for every future optional stage the same way.
- **8.3** — "Optional" is a property declared by the subsystem, not discovered by the caller catching exceptions ad hoc. A stage that can degrade should expose an explicit `is_available()`-style check (as `embeddings.rs` already does) so callers make an intentional decision, not an accidental one via error-swallowing alone.

---

## 9. Bounded contexts

Lumos has three bounded contexts. Terms mean the same thing only within a context; crossing a context boundary requires an explicit translation, never an assumed shared meaning.

| Context | Owns | Example terms | Talks to other contexts via |
|---|---|---|---|
| **Ingestion** | Turning raw OS/user events into stored Objects. One bounded context per source *type* internally (Files, Clipboard, ...) but all shaped identically (§2). | "event", "raw payload", "classification" | Produces Objects (§1) — the only artifact other contexts see |
| **Search & Ranking** | Turning a Search Intent into a ranked, deduped result list over Objects. | "SearchObject", "Intent", "score", "candidate" | Consumes Objects + their SearchProvider adapters; never reaches into Ingestion internals |
| **Presentation** | Rendering results and accepting user input; owns nothing persistent. | "result card", "highlight", "selection" | Calls Core's command surface; never touches Objects, SQL, or ranking directly |

- **9.1** — "Metadata" means different things in different contexts and that's fine *inside* a context, but the shared Object envelope (§1.2) is the one vocabulary all three contexts agree on at the boundary. A context is free to have richer internal concepts as long as what crosses the boundary is expressed in that shared vocabulary.
- **9.2** — No context reaches past another context's boundary to grab data "because it's faster." If Presentation wants ranking internals for a debug view, that's a new, explicit API on Search's boundary — not a direct read of Search's internal candidate list.

---

## 10. Data ownership

- **10.1** — Each Object type has exactly one owning module responsible for its full lifecycle (create, update, delete, and cleanup of anything derived from it — embeddings, index entries, thumbnails). No other module writes to that Object's storage directly.
- **10.2** — Derived data (embeddings, OCR text, thumbnails, FTS index entries) is owned by the pipeline stage that produces it, but its *lifecycle* is bound to the source Object's lifecycle (§1.5) — when the Object is deleted, all derived data for it is deleted too, enforced by the owning module, not by a separate best-effort sweep job. (Today's clipboard retention purge does this correctly for embeddings; this must be the pattern for every future derived-data type, not a one-off.)
- **10.3** — The database schema is an implementation detail of Infrastructure. No other module (Search, UI, a source plugin) assumes specific table names or column layouts belong to it beyond the interface Infrastructure exposes. This is what makes "swap SQLite's brute-force cosine scan for sqlite-vec or an ANN index later" a change confined to one place instead of a cross-cutting rewrite.

---

## 11. What this document is not

- It is not a justification for building abstractions the current two sources don't need yet. `SearchProvider`, the plugin registry, and the generic `Object` model should be *introduced* when the second real consumer of the abstraction shows up (Screenshots, most likely) — not before, and not as a big-bang rewrite of Files/Clipboard "just in case." Premature generalization is exactly the kind of decision Phase 9 of the code review already warns about.
- It does not mandate a specific ORM, framework, or library. It mandates the *shape* of dependencies and responsibilities, independent of implementation choice.
- It does not replace `LUMOS_DESIGN.md` (the product vision and feature roadmap) — it constrains *how* that roadmap gets implemented, whatever order the features land in.

---

## 12. Known deviations from this document today

Documented explicitly so drift is visible instead of silent. None of these need fixing as a side effect of adding this document — they are tracked debt, to be closed as each relevant module is next touched.

### Phase 4 — Generic Search: decisions made along the way

- ~~`search.rs` imports and matches on `clipboard::ClipboardResult` and file records directly~~ — **closed.** `search_all`'s retrieval/merge/RRF/dedup core never names `File`/`Clipboard`; it iterates `SearchIntent.object_types` through `FullTextIndex`, `EmbeddingStore`, `ObjectStore::recent`, and `ObjectStore::load` only. `search.rs` has no `use rusqlite::Connection` in production code (only in test helpers, which construct a concrete `SqliteStore` the same way `storage_sqlite.rs`'s own tests do).
- ~~Search's current output is concatenated per-source blocks, not a merged+ranked list~~ — **closed.** One `HashMap<ObjectId, f32>` candidate pool, RRF-fused, ranked once, limited once.
- **Two bounded, documented exceptions to "generic search never reads payload"** exist in `search.rs`, both narrower instances of the same justified pattern: `matches_detected_type` (reads `ClipboardPayload::detected_type` — the only way to reproduce `type:xxx`'s pre-existing clipboard-only semantics, since `ObjectType` restriction already excludes files whenever it applies) and `exact_match_boost` (reads `FilePayload::filename`/`ClipboardPayload::text_content` — exactness inherently requires content). Neither generalizes; both default to a neutral value for object types they don't recognize, so adding Screenshot/Email/BrowserVisit does not require touching either function.
- **Pin/boost decision: Option B taken, by default rather than by new judgment.** `pinned` stays on `ClipboardPayload`, not promoted to `ObjectMetadata`, and does **not** participate in generic ranking. This reproduces the pre-Phase-4 engine exactly — pin never affected ranking before Phase 4 either — so it is not a behavior regression, only a signal this phase declines to add. Revisit if pin-boost is ever actually wanted; that still requires the metadata-vs-payload model decision this document already flagged, unresolved.
- ~~`since`/`detected_type` filtering happens post-hydration, with a documented approximation~~ — **the `recent()`-sourced half of this is closed; the FTS/semantic half is not, and is not closable the same way.** `since`, `until`, and `detected_type` are all now enforced inside `ObjectStore::recent`'s SQL, before its own `LIMIT` — see the two Phase 4 post-validation entries below. What's left unfixed, deliberately: `FullTextIndex::search_text`/`EmbeddingStore::nearest` still take no filter parameters, so FTS/semantic candidates are still over-fetched (`CANDIDATE_OVERFETCH = 3`) and filtered only after hydration. That path was never observed to actually fail the way `recent()` did (its candidate pool is bounded by real content relevance, not purely by insertion order), and extending either trait remains unjustified scope without a demonstrated failure.
- **Phase 4 post-validation, pass 1 — `recent()` + `until`.** A dedicated validation pass (real CLI queries, real live database, not just the test suite) found `until` had the *same* bug shape as the original `since` design: enforced only after `recent()`'s SQL had already run its own `LIMIT`. Reproduced with a controlled fixture (70 "today" rows + 3 "yesterday" rows, `fetch_n=60`) — all 3 genuine matches vanished. Fixed by moving `until` into the `WHERE` clause itself, ahead of `LIMIT`. `matches_time_bounds` still runs afterward as defense-in-depth for FTS/semantic candidates, which have no time filter at retrieval at all.
- **Phase 4 post-validation, pass 2 — `recent()` + `detected_type`.** The same validation discipline applied a second time to `type:jwt` (deliberately not fixed in pass 1 — different filter, out of scope for that turn) found the *identical* bug shape: `detected_type` was enforced only in `matches_detected_type`, after `recent()`'s `LIMIT` had already run. This was not hypothetical — confirmed failing on the real live database (unrelated clipboard growth pushed the 60-most-recent-id cutoff past all three genuine `jwt` rows, so `search-all type:jwt` returned nothing on the actual running system). Fixed identically: `detected_type` moved into `recent()`'s `WHERE` clause, ahead of `LIMIT`, ignored (not erroring) for `File`, which has no such column — the same "no opinion" rule `matches_detected_type` already applied at the domain layer. `matches_detected_type` still runs afterward, same defense-in-depth reasoning as `matches_time_bounds`.
- **`ObjectStore::recent` added** — a third retrieval kind, structurally distinct from FTS/semantic (no query to match or embed against), for the text-less `today`/`yesterday`/`week`/`type:xxx`-alone browse mode. Deliberately never RRF'd against FTS/semantic candidates (mutually exclusive triggers in practice: recency mode only fires when there's no text). Caught a real regression during implementation: the first version only triggered recency mode on `since`, and `type:xxx` alone (no `since`, no other text) silently returned zero results where the pre-Phase-4 `clipboard::search` returned matches — found by the CLI smoke test this document's own process requires after each major step, fixed before being reported as done.
- The schema itself is still source-oriented (unchanged by this phase, matching earlier scope decisions — see the Phase 3 entry above): `files`/`clipboard_entries` remain separate tables; `ObjectStore::recent`/`load`/`search_text` dispatch on `ObjectType` internally in `storage_sqlite.rs`, which is exactly where §10.3 says that dispatch belongs.
- `clipboard::by_id` and `search::file_by_id` were deleted — both were solely the old `search_all`'s per-candidate hydration helpers (N+1 by construction), fully superseded by `ObjectStore::load`'s batched call. Confirmed dead via workspace grep before removal, per the brief's explicit "don't delete blindly" instruction.
- `clipboard::search`/`recent`/`set_pinned`/`purge_older_than` and the CLI's standalone `lumos clipboard search`/`lumos search` commands are **unchanged in spirit** — `search()` (the plain smoke-test command) was rewritten only enough to drop `Connection`, staying FTS-only/no-RRF exactly as before; the clipboard-specific commands were explicitly out of scope and still take `&Connection` directly, which is fine — Phase 4's acceptance criteria are about the generic engine, not a blanket ban on `Connection` everywhere in `clipboard.rs`.

- **Phase 4 post-validation, pass 3 — semantic relevance / ranking inversion.** The earlier `RELEVANCE_MARGIN` fix stopped semantic search padding results to `K`, but could not stop a genuinely irrelevant object outranking a relevant one — the margin is measured *from the top*, so it cannot tell whether the top itself is meaningful. Root cause, established by measuring the real model against the real corpus rather than reasoning about it: embedding anisotropy makes absolute cosine nearly uninformative for this corpus. Two queries measured 0.0001 apart in top score — `"jwt"` (0.8579, noise: `jp.png`) and `"database connection configuration"` (0.8578, genuine: `IdentityServerConfigurationDbContext.cs`) — proving no absolute threshold can separate signal from noise. What *does* separate them is the shape underneath the top: genuine relevance is a sharp peak, noise is a flat plateau where the "best" hit is merely the maximum of an undifferentiated crowd. Fixed by adding a concentration test (`MAX_TOP_BAND_FRACTION = 0.05` in `embeddings.rs`): if more than 5% of the corpus sits within `RELEVANCE_MARGIN` of the top score, the entire semantic list is rejected as uninformative. Measured separation across both corpora and nine query classes was clean — genuine 0.3–3.0%, noise 6.6–37.4% — with 5% placed in the middle of the gap. Required one new storage method (`EmbeddingStore::vector_count`) to supply the corpus-size denominator; counting rows is a storage question, the judgment stays in the domain. Deliberately not keyword-, shape-, or magnitude-based: the rule reads only the *distribution*, so it applies identically to acronyms, natural language, identifiers, paths and no-match queries. Corpora under 20 vectors are exempt (the statistic is meaningless at that size).
- **A second, deeper limitation this exposed and did *not* fix:** file embeddings are computed from the **filename alone**, never file content (`indexer.rs` passes `filename` to `ingest`). The reported reproduction compared the query `"jwt"` against the string `"NotificationsTable.jsx"` — the model never saw the file's contents. Semantic file search is therefore inherently weak regardless of any relevance rule; the concentration fix suppresses the resulting false positives but cannot create signal that was never indexed. Embedding file content is a separate, larger change (extraction, size limits, chunking) and is not attempted here.

### Phase 5, Step 5.1 — the LLM-facing intent schema

- **The schema contains no timestamp and no id field, structurally.** §5.2 says the model fills a schema rather than generating a query language; applied to time and identity that means the model emits a *label* (`TimeToken`, closed set) and a project *name*, and deterministic code does every piece of date arithmetic and every id lookup. This keeps `SearchIntent`'s contract rule 1 ("values arrive already resolved") satisfiable without asking an LLM to know what "now" is, and keeps the project name→id database read on the deterministic side of §5.1. `#[serde(deny_unknown_fields)]` makes it enforcement rather than convention — a response inventing `"since": 1786451940` is rejected outright, proven by `raw_timestamps_and_ids_are_structurally_unrepresentable` (verified to fail without the attribute).
- **The schema offers only sources that are actually searchable.** Not tidiness: `retrieve_candidates` returns `Err` for any `ObjectType` the store can't serve, so a model answering "screenshots" would fail the *entire* search rather than return fewer results. `SourceName` therefore lists only `File`/`Clipboard`, and `source_name_set_matches_searchable_types` locks it to `all_object_types()` so the two cannot drift when a new source ships (verified to fail when they diverge).
- **Model-facing field names avoid the word "type" entirely** (`sources`, `content_kind`). `ObjectType` and `detected_type` are unrelated axes that already get confused in prose, and "type" is exactly the name a model reaches for as a polymorphic discriminator. The renaming is confined to the JSON schema — the Rust field names are unchanged, per the earlier decision that renaming them wasn't worth the churn.
- **Calendar-anchored time expressions are deliberately absent from v1** ("between Monday and Friday", "after 10 August"). They need timezone handling this codebase explicitly does not have — the same reason `parse_filters` uses rolling offsets rather than midnight boundaries. Offering them in the schema would promise something the resolver cannot honour. `Today`/`Yesterday`/`LastWeek` resolve identically to the CLI parser's labels, asserted by `time_tokens_match_the_cli_parser_for_shared_labels`, so both producers of a `SearchIntent` agree.
- **An unresolvable project name yields `project_id: None` rather than an error.** Safe because `project_id` is a ranking boost, not a hard filter (established in Phase 4) — so an unknown name degrades ranking slightly instead of wrongly excluding every result.
- **Nothing in this step is wired into the running application yet.** The module is pure types and pure functions with no caller; the release binary was still rebuilt and relaunched, and Phase 4 behavior re-verified on live data, rather than assuming "no caller" means "no need to check".

### Phase 5, Step 5.2 — the interpreter boundary + deterministic fallback

- **`interpreter::SchemaInterpreter` is the entire LLM-facing surface, and its shape is what makes the boundary structural rather than conventional.** One method, `generate(&self, query: &str) -> Result<String>` — no parameter through which a storage handle, a `Connection`, or a `SearchIntent` could arrive, and no return type other than raw text. An implementor cannot hand back a `SearchIntent` directly even if it wanted to; there is nowhere in the signature for one to go.
- **`interpreter::interpret` is the only path from that raw text to a `SearchIntent`, and it always goes through `intent_schema::parse_schema_json`.** No second, shorter conversion exists. Every failure — no interpreter configured, `generate` erroring, non-JSON text, a schema version/source/token that doesn't validate — collapses to the same outcome: fall back to `search::parse_intent` on the original query. Confirmed by temporarily replacing the two fallback checks with `.unwrap()`: 4 of the 8 tests failed (panicked) as designed, then the checks were restored.
- **This confirms §5.3 (fallback) is enforced by this function's own control flow, not by a caller remembering to catch an error.** Any future concrete `SchemaInterpreter` (Ollama in Step 5.3, anything else later) inherits the fallback for free — it cannot bypass it by construction, only by `interpret` itself changing.

### Phase 5, Step 5.3 — the Ollama-backed `SchemaInterpreter`

- **`ollama::OllamaInterpreter` is a transport-only `SchemaInterpreter`: it returns raw model output and has no access to storage, search, payloads, or `SearchIntent`.** Confirmed both structurally (the module's own `use` list contains no `rusqlite`, `storage`, `storage_sqlite`, or `search_all`/`SearchIntent`) and by `SchemaInterpreter`'s signature itself (§5.2 of Step 5.2, above) — there is no parameter or return type through which any of those could reach this module. Validation (`intent_schema::parse_schema_json`) and deterministic fallback (`interpreter::interpret`) both remain outside the client, exactly where Step 5.2 put them.
- **Ollama is the design's own choice, not a Step 5.3 decision.** `LUMOS_DESIGN.md` §5/§6 already specifies "AI Interpreter: Ollama (HTTP, localhost)" and "Отдельный локальный HTTP-процесс, не встроен в Rust-ядро" — this step implements that, it does not choose between embedding an inference runtime versus calling a local HTTP process.
- **The prompt is generated from `intent_schema.rs`'s own constants (`SCHEMA_VERSION`, `SourceName`, `TimeToken`), not retyped as a second copy of the same vocabulary.** This is guidance text for the model, not an enforcement mechanism — a model that ignores it still only ever produces a `String`; `parse_schema_json` remains the sole authority on whether that string is acceptable, unchanged from Step 5.2.
- **`ureq`'s existing `json` feature was enabled rather than adding a second HTTP dependency.** `ureq` (already a dependency, used for the embedding model download in `embeddings.rs`) supports a localhost JSON POST with a per-request timeout once its optional `json` feature is on; `reqwest` or any async runtime would have been unjustified scope for a single blocking local call.
- **No HTTP-mocking dependency was added.** The repository had none, and the client's tests use a minimal loopback `TcpListener`-based server (accept one connection, record the request, write a fixed response, close) rather than a live Ollama installation — offline and deterministic, per the same "no network in the test suite" principle `embeddings.rs`'s own tests already follow for the embedding model.

### Phase 5, Step 5.4 — CLI/app wiring (brief, for context)

- **`ProjectStore` gained a read-only `find_project_by_name`, distinct from `get_or_create_project`.** The existing method *creates* a project on a miss — correct for `project add`, wrong for resolving an LLM-supplied name, where a hallucinated or misspelled name must degrade to "no project filter," never silently create an empty project as a side effect of a wrong guess. `find_project_by_name_never_creates_a_project` asserts the `projects` row count is unchanged on a miss, not just that the return value is `None`.
- **`interpreter::search_all_interpreted` is the one shared composition CLI and app both call** — `resolve_project` built from the read-only lookup above, then `interpret()` (Step 5.2, unchanged), then `search::search_all` (unchanged). `search::search_all_text` is untouched and still means exactly what it always has; this is a second entry point, not a replacement.
- **CLI config: global flags (`--no-llm`, `--ollama-url`, `--ollama-model`, `--ollama-timeout-ms`), matching the existing `--db` convention. App config: `LUMOS_*` environment variables**, matching how `LOCALAPPDATA` is already read elsewhere (`db.rs`, `embeddings.rs`) — no settings file, no TOML, no new dependency, per explicit scope.

### Phase 5, Step 5.5 — process-lifetime availability state

- **A live-but-unreachable Ollama must not cost every query the same connection-attempt latency.** Measured on a machine with no Ollama installed: ~4.2s per query with the interpreter enabled (real transport-level connection failure), ~0.15s with `--no-llm`. Fixed with one `AtomicBool` field (`unavailable`) on `OllamaInterpreter` itself — checked at the top of `generate()`, set only by a transport-level failure (`ureq::Error::Transport`, never `Error::Status`). One instance is constructed once per process (CLI: in `main()`; app: once in `setup()`, held in `OllamaState`), so "process-lifetime" and "this struct's lifetime" are the same span — the flag needs no explicit reset: a new process means a new instance means `unavailable` starts at `false` again.
- **Inspected before choosing this:** `embeddings.rs`'s `static EMBEDDER: OnceLock<Option<Embedder>>` is the closest existing pattern but doesn't fit — it memoizes a value computed *once, ever*, whereas Ollama's success path must still be attempted fresh every query; only the transport-failure path is sticky. The app's own `AtomicBool` idiom (`SuppressBlurHide`) is the actual analog: one sticky boolean, interior-mutable, unrelated to any startup probe. The check still only ever runs inside `generate()`, on a real query — never at `setup()`/`main()` — so this does not reintroduce the startup-probing this phase has repeatedly ruled out.
- **The three-way distinction is enforced by *where* the flag is set, not by an extra check.** Connection refused/DNS failure/timeout (`Transport`) → marks unavailable. A real HTTP response with a bad status (`Status`), a malformed body, or empty generated content → never touches the flag; these happen in a different branch entirely (after a successful `send_json`), so an alive-but-misbehaving Ollama is structurally distinguished from an absent one, not just by convention. Confirmed by two deliberate regressions: marking `unavailable` on *any* error broke `http_error_status_does_not_mark_ollama_unavailable`; moving that same mistake into the body-parse branch broke `malformed_json_response_does_not_mark_ollama_unavailable` too. Both reverted.
- **Known, accepted limitation: this fixes the app, not the CLI's repeated-invocation cost.** Each `lumos.exe search-all` invocation is its own process, so `unavailable` starts at `false` again every time — measured directly: two separate CLI invocations with Ollama absent cost ~4.2s *each*, unimproved by this step. Within one process (the Tauri app's actual usage pattern — many searches per app session), the fix works exactly as designed: first call ~4.08s, every subsequent call in that process 1–3µs, matching `--no-llm`'s cost. A CLI-specific fix (a much shorter first-attempt timeout, or defaulting `--no-llm` unless explicitly enabled) is a product decision for a later step, not resolved here.
- **Real Ollama inference remains unverified end-to-end.** No Ollama installation exists on the development machine this phase was built and tested on; per explicit instruction, none was installed to manufacture a validation. What *is* verified thoroughly is the availability/fallback state machine itself, via a real (test-local, offline) TCP server and real transport failures — not simulated, not timing-only (`accepted_count()` on the mock server is the primary assertion in every new test; timing is supplementary evidence per this step's own instruction).

### Phase 5, Step 5.6 — validation pass (no code changed)

- **The LLM boundary (§5.1/§5.2) was re-verified structurally, independent of the tests that originally proved it.** `ollama.rs`'s own `use` list contains no `rusqlite`/`storage`/`storage_sqlite`/`search_all`; `interpreter.rs` depends only on `storage`'s abstract traits (`ProjectStore`, `ObjectStore`, ...) and `search::SearchIntent`, never on a concrete store; no ingestion/pipeline/scan/watcher/indexer module references `interpreter::` or `ollama::` at all. Confirmed by direct grep across `crates/lumos-core/src`, not inferred from the design.
- **Ollama is still not installed on the machine this project is developed on.** Checked again this pass (`curl localhost:11434/api/version`, process list, `which ollama`) — still absent. Real end-to-end model inference (a live Ollama generating a schema-valid `SearchIntent` that reaches `search_all`) **remains unverified** and was not simulated to manufacture a pass; this is the one open item blocking full confidence in Phase 5, not a code defect.
- **The fallback-equivalence property was re-confirmed live, not just in unit tests.** On the current release binary, with Ollama absent: `search-all <query>` (LLM enabled, falls back) and `search-all --no-llm <query>` (LLM never attempted) produce byte-identical output for both a filter query (`type:jwt`) and a free-text query — the model-unavailable path and the explicitly-disabled path are indistinguishable in their result, as designed.
- **The CLI/app latency split documented in Step 5.5 was re-measured, not just recalled.** Two independent CLI invocations with Ollama absent: ~4.2–6.9s each (varies with query shape — semantic search's cold-start cost adds to simple filter queries), unimproved by the availability cache, exactly as Step 5.5 predicted for a one-shot process. This is restated here only because it was re-measured this pass, not because it changed.
- **No new architectural decision was made in this step.** This entry exists to record that a validation pass happened and what it found, per this phase's own instruction to update the document "only with decisions that are now actually validated" — the decisions themselves are all Step 5.1–5.5's, unchanged.

### Phase 5 — SearchIntent contract prework: decisions made along the way

- **`SearchIntent.until: Option<i64>` added** — the one field the Phase 5 analysis proved necessary by concrete counter-example ("before yesterday", "between Monday and Friday" are structurally unexpressible with a lower bound alone). No `TimeRange` wrapper: `since`/`until` are independent optional epochs, consumed by one generic `matches_time_bounds` check — a wrapper would add indirection without adding safety, since both fields are still `Option<i64>` either way.
- **Deliberate, disclosed behavior change: `yesterday` now sets both `since` and `until`, narrowing to a genuine ~24h window one day back**, rather than the old since-only "at least 48h ago" (which silently *included* today's results too — a superset, not what the word means). `today`/`week` are unchanged (open-ended up to now; there's no meaningful upper bound distinct from "now" for either). Verified via CLI smoke test that this only changes `yesterday`'s result set, not the others.
- **`clipboard::Filters` gained the same `until` field, but `clipboard::search`'s SQL never reads it** — deliberate: extending `Filters` was the only way to give `parse_intent` an `until` value without duplicating `today`/`yesterday`/`week` token-scanning logic in a second function, and since `#[derive(Default)]` already covers every other `Filters` construction site, the standalone `lumos clipboard search` command's behavior is unchanged by construction, not by care taken at each call site.
- **`object_types` empty-vector footgun closed.** `all_object_types()` is the one spelling of "search everywhere". `search_all` returns an `Err` if `intent.object_types` is empty, rather than the pre-Phase-5 risk of silently iterating zero times and returning zero results — indistinguishable from "nothing matched," and a plausible mistake for an LLM interpreter encoding "no restriction" as `[]` instead of the explicit list. (As of Phase 6 Step 6.2, `all_object_types()` no longer holds its own exhaustive match — see that entry below; this bullet's claim about `search_all`'s empty-vector error is otherwise unaffected and still holds.)
- **`search_all` split into `search_all(store, &SearchIntent, limit)` (the real engine) and `search_all_text(store, raw_query, limit)` (thin CLI wrapper: `parse_intent` then delegate).** This is the prerequisite a future LLM interpreter needs — it can call `search_all` with its own constructed `SearchIntent` and never touch a query string. No `SearchProvider`/`LLMSearchEngine` abstraction introduced; the split is two functions, one delegating to the other.
- **Two natural-language gaps identified but deliberately not addressed by new fields**, per the prior analysis: a hard `pinned` filter ("pinned things about SQL") remains inexpressible — still blocked on the unresolved pin metadata-vs-payload model question, now with a concrete trigger example rather than a hypothesis; and adjustable ranking-weight preferences ("most recent" as an explicit override rather than the fixed default recency boost) were evaluated and deferred — no example demonstrated a case where the fixed default actually produces a wrong answer, only a suboptimal one.
- Source-specific structured filters (email sender, browser URL, ...) remain explicitly excluded from `SearchIntent` by contract rule 3 in its doc comment — they degrade to free-text `text`, not a new field per source, which is what keeps this struct from becoming "one field per source" as more sources are added.

### Phase 6, Step 6.2 — the source registry (minimal, non-speculative)

- **One authoritative list, `registry::REGISTERED_OBJECT_TYPES`, replaces two independently hand-maintained copies of the same fact.** Before this step, `search::all_object_types()` and `storage_sqlite::SqliteStore`'s private `SUPPORTED` constant each listed `[File, Clipboard]` by hand, in two different files, with nothing enforcing they stayed in sync — they agreed by coincidence, not by construction. Both now read the same constant; `all_object_types()` is a one-line delegation, `supported_types()`/the unsupported-type error message reference it directly.
- **Deliberately not a trait, not a plugin system, not runtime registration.** Inspected first (Phase 6 Step 0/6.1): nothing about File or Clipboard varies in a way a trait method would express — both already go through identical generic calls (`FullTextIndex`, `EmbeddingStore`, `ObjectStore`) dispatched purely on `ObjectType`. The only duplicated fact was *which types exist*, and a `const` array is the smallest thing that can hold one fact. Introducing `SourceProvider`/`SearchProvider` traits now, with still only two real sources, would have been exactly the premature generalization §11 already warns against — just relocated from "wait for a third source" to "wait for a second behavior that actually differs" (which also hasn't happened).
- **The compile-time "fail loudly on a new variant" property moved, not disappeared.** `all_object_types()` used to contain its own exhaustive `match` over `ObjectType`, so an unclassified new variant failed to *compile*. A plain `const` array cannot do that. `registry.rs`'s own test (`every_object_type_is_deliberately_classified`) restores the guarantee at the test layer instead — same pattern already used for `intent_schema::SourceName` vs `all_object_types()` in Phase 5.1. Verified by deliberately misclassifying `Screenshot` as registered without adding it to the array: the test failed (`left: 3, right: 2`); reverted.
- **`storage_sqlite.rs`'s `type_tag`/`source_table` were left untouched, on purpose.** Those encode SQL-specific facts (on-disk discriminator strings, table/column names) that have no shared representation across types — folding them into the domain-level registry would add indirection without removing any real duplication of *meaning*. Only the "which types are registered at all" fact was unified; per-type SQL specifics stay exactly where `storage_sqlite.rs`'s own module doc says infrastructure concerns belong (§10.3).
- **Everything else named in `ARCHITECTURE_INVARIANTS.md`'s existing Phase 4 deviation entry below remains open, not closed by this step.** `matches_detected_type`, `exact_match_boost`, and `to_search_hit` still read payload directly for File/Clipboard specifically — unchanged, since both are either a documented bounded exception or (`to_search_hit`, `SearchHit`) explicitly permitted UI-adjacent code per §7.1. This step closed only the narrow "which `ObjectType`s exist" duplication, not the broader `SearchProvider` seam.

### Phase 7, Step 7.1 — V8 dedup (foreground-process lookup)

- **`apps/lumos-app/src-tauri/src/win32.rs`'s `foreground_exe_name()` is now the one place that owns Win32 GetForegroundWindow → PID → OpenProcess → QueryFullProcessImageNameW.** `lib.rs::foreground_process_name` and `clipboard_watcher.rs::foreground_app_name` were byte-for-byte identical implementations of this lookup (confirmed by direct diff during Phase 6's Step 0 inspection), differing only in what each did with the raw exe name afterward — `lib.rs` lowercases it for `PICKER_PROCESS_NAMES` matching, `clipboard_watcher.rs` maps it through `friendly_app_name` for display. The shared function returns the raw, case-preserved exe name; each caller keeps its own post-processing, so behavior is byte-for-byte unchanged — confirmed via full workspace test pass and live Phase 4/5 regression smoke checks after the change.
- This closes V8 from `PHASE0_ARCHITECTURE_ALIGNMENT.md` — pure DRY, no behavior change, no new abstraction (a plain function in a new module, not a trait).

### Phase 7, Step 7.2 — V9: deleted `models.rs`

- **`crates/lumos-core/src/models.rs` and `object_adapters::file_record_to_object` (+its test) are deleted.** Confirmed by grep, immediately before deleting: `Project`/`ProjectPath` had zero references anywhere in the workspace; `FileRecord` had exactly one caller, `file_record_to_object`, which itself was reachable only from its own test — no production call site in `indexer.rs`, `search.rs`, or anywhere else, through Phases 1–6.
- **Not kept as scaffolding.** This was written in Phase 1 as a speculative future adapter ("no existing call site is wired to them yet," per its own now-deleted doc comment) and no real call site ever appeared. Per this project's own standing rule (`LUMOS_STORY.md`, "don't introduce an abstraction before its second consumer" — the inverse failure mode: don't *keep* one that never got a first consumer either), this is deletion, not preservation.
- This closes V9 from `PHASE0_ARCHITECTURE_ALIGNMENT.md`.

- ~~`search.rs` imports and matches on `clipboard::ClipboardResult` and file records directly — violates §4.1. No `SearchObject`/`SearchProvider` seam exists yet; today's two sources are hardcoded into Search. Acceptable for now per §11 (only two sources exist), must be resolved before a third source (e.g. Screenshots) is added, not after.~~ — **superseded by Phase 4's entry above and Phase 6's post-6.2 audit; re-stated precisely rather than left stale.** This line predates both and no longer describes the code accurately. Two distinct things were bundled under one "violates §4.1" label; they resolved differently:
  - The part that was genuinely core-logic coupling — `search.rs` maintaining its own hand-written File/Clipboard list for "which types does generic search iterate over" — is **closed**, first by Phase 4 (retrieval/merge/RRF/dedup never named a concrete type, see the entry above) and finally by Phase 6 Step 6.2, which removed the last hand-written copy of that list (`all_object_types()`'s inline match) in favor of `registry::REGISTERED_OBJECT_TYPES` — the one place both `search.rs` and `storage_sqlite.rs` now read it from.
  - The part that is **not** closed and was never a defect to begin with: `search.rs` still imports `clipboard::ClipboardResult` and reuses it as `SearchHit::Clipboard`'s payload, and `to_search_hit` still matches `File`/`Clipboard` by name to build a `SearchHit`. This is source-specific code at the presentation/result-shaping boundary, not the retrieval/ranking core — exactly the "new result-kind branch in the display layer" §7.1 names as the one thing beyond registration a new source may legitimately require. Re-classified as a **bounded, explicitly allowed exception**, not outstanding debt.
  - Not claimed: that File/Clipboard-specific code has disappeared from `search.rs`. It hasn't, and per §7.1 isn't expected to — `SearchHit`, `to_search_hit`, `matches_detected_type`, and `exact_match_boost` all still name concrete types, each already individually justified (see the Phase 4 entry above and Phase 6 Step 6.2's own entry). What's actually established is narrower and real: the one list controlling *which types generic search considers* now has exactly one authoritative source, not two independently-hand-maintained ones.
- ~~Search's current output is concatenated per-source blocks, not a merged+ranked list — violates §4.3/§4.4.~~ — **stale; closed by Phase 4.** This line predates Phase 4 and was never pruned after landing. See the entry near the top of this section: one `HashMap<ObjectId, f32>` candidate pool, RRF-fused, ranked once, limited once — not per-source blocks. Confirmed still true by Phase 7's Step 7.1 re-audit (no re-inspection needed beyond re-reading `search_all`; nothing touched it since).
- ~~Core (`lumos-core`) passes `rusqlite::Connection` directly through pipeline-stage-shaped functions rather than through a storage trait~~ — **partly closed in Phase 2, reshaped in Phase 2.1; the `search.rs` half also closed by Phase 4, not still open as this line used to claim.** `indexer`, `scan`, `watcher`, `metadata` and `embeddings` depend only on the traits in `storage.rs`; `storage_sqlite.rs` is the sole implementation and the only place that dispatches on object type. `search.rs` has zero `Connection` references in production code today (confirmed by direct grep during Phase 7's Step 7.1 re-audit — only a test helper constructs one, the same way `storage_sqlite.rs`'s own tests do). **Still genuinely open:** the read/lifecycle half of `clipboard.rs` (`search`, `recent`, `set_pinned`, `purge_older_than`) still takes `&Connection` directly — `by_id` was deleted in Phase 4, the other four were not.
- **§10.2 has a live bypass.** `ObjectStore::delete` removes an object *and* its derived data, so callers can't forget. But `clipboard::purge_older_than` still deletes rows with raw SQL and runs its own orphan-embedding sweep, so there are two deletion paths and only one enforces the rule. Until retention is routed through `delete`, §10.2 is enforced-by-default rather than enforced-by-construction. Close when `clipboard.rs`'s lifecycle half moves behind the boundary (Phase 4).
- ~~The storage boundary is write-shaped only~~ — **closed as Phase 4 prework.** `ObjectStore::load(&[ObjectId]) -> Vec<Object>` and a new `FullTextIndex::search_text` trait now exist and are tested in `storage_sqlite.rs`. Neither is *wired into* anything yet — `search.rs` still hydrates via its own `file_by_id`/`clipboard::by_id` and still runs its own hand-written FTS queries. The capability exists; the migration of `search.rs` onto it is Phase 4 itself, deliberately not done here.
- ~~There is no explicit `Object` envelope type~~ — **closed in Phase 3.** `indexer::index_file` and `clipboard::record_text` now construct a real `NewObject` and pass it through the shared `pipeline::ingest`, not just adapters/tests. `files` and `clipboard_entries` are still two separate tables underneath (that part of §1 stays a target, not a fact — see the next point), but production ingestion genuinely speaks `Object` now.
- The schema itself is still source-oriented: `files` and `clipboard_entries` remain independent tables with independently-named "created_at"-equivalent columns; `embeddings` is still the only table with a real `(object_type, object_id)` identity scheme. Phase 3 was explicitly scoped to not touch this (no schema redesign) — closing this gap fully means either a shared `objects` table or accepting per-source tables as a permanent, storage-internal detail the `ObjectStore` boundary already hides.
- Relations (§1.4) don't exist as a concept anywhere in the schema yet — there is no `session_events`-equivalent table. Expected at this stage (Sessions isn't built); the concrete first place §1.4 will be tested.
- **Store and Index are physically fused for the two sources that exist.** `ObjectStore::save` and the `files_fts`/`clipboard_fts` trigger-driven sync happen inside one SQL statement — there is no separate "index it" call today. Documented in `pipeline.rs` rather than papered over with an unused `IndexStore` trait; revisit only if a future source's search backing isn't SQLite FTS.
- **Clipboard's Watch/Parse stages live outside `lumos-core`** (`apps/lumos-app/src-tauri/src/clipboard_watcher.rs`), unlike the file watcher which lives inside core (`watcher.rs`). Both are legitimately OS-specific per §2.3, but the asymmetry in *which crate* hosts them predates Phase 3 and wasn't in scope to fix.
- **`clipboard::purge_older_than` bypasses `ObjectStore::delete`** — same bypass noted after Phase 2.1, still open. Retention deletes rows and sweeps orphaned embeddings with raw SQL instead of going through the boundary that would do both atomically per object.
