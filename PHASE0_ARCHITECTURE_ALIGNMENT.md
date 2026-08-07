# Phase 0 — Architecture Alignment

**Scope:** analysis only, per [ARCHITECTURE_INVARIANTS.md](ARCHITECTURE_INVARIANTS.md). No code changed in this pass.
**Goal:** map what exists today, name every place it disagrees with the invariants, and sequence the migration so each phase (1-7) is independently shippable and never requires a big-bang rewrite.

---

## 1. Current architecture map

### 1.1 Modules and responsibilities

| Module | Location | Responsibility (as implemented today) |
|---|---|---|
| `db` | `crates/lumos-core/src/db.rs` | Owns schema DDL, migrations, `open()`/`default_db_path()`. The only place that knows table shapes. |
| `models` | `crates/lumos-core/src/models.rs` | Declares `Project`/`ProjectPath`/`FileRecord` structs. **Dead** — unused everywhere else. |
| `metadata` | `crates/lumos-core/src/metadata.rs` | Project registration, repo-type detection, `resolve_project_for_path()` (path → project_id resolution). |
| `indexer` | `crates/lumos-core/src/indexer.rs` | Single entry point for representing a file in storage: stat, hash, resolve project, upsert `files`, trigger embedding. |
| `scan` | `crates/lumos-core/src/scan.rs` | Bulk directory walk, calls `indexer::index_file()` per file. |
| `watcher` | `crates/lumos-core/src/watcher.rs` | Live FS event loop (`notify`), dispatches to `indexer`. |
| `clipboard` | `crates/lumos-core/src/clipboard.rs` | Everything clipboard: classification, dedup, storage, search, retention, pin. |
| `embeddings` | `crates/lumos-core/src/embeddings.rs` | Model download/cache, ONNX inference, mean-pool/normalize, vector storage, brute-force cosine search. |
| `search` | `crates/lumos-core/src/search.rs` | FTS query building, filter parsing, and `search_all()` — the actual merge point of FTS + semantic, files + clipboard. |
| `lumos-cli` | `crates/lumos-cli/src/main.rs` | Thin CLI wrapper calling the above directly. |
| Tauri commands | `apps/lumos-app/src-tauri/src/lib.rs` | Tauri command handlers, app setup/wiring, Win32 foreground-process lookup, focus/hotkey/autostart glue. |
| `clipboard_watcher` | `apps/lumos-app/src-tauri/src/clipboard_watcher.rs` | Live clipboard poll loop, its own DB connection, own copy of foreground-app lookup. |
| `hotkey` | `apps/lumos-app/src-tauri/src/hotkey.rs` | `WH_KEYBOARD_LL` hook, Ctrl-double-tap detection. |
| `autostart` | `apps/lumos-app/src-tauri/src/autostart.rs` | Startup `.lnk` management. |
| `app.js` | `apps/lumos-app/dist/app.js` | All frontend rendering, selection, filter-autocomplete, add-project flow. |

### 1.2 Dependency direction (as it actually is)

```
lumos-cli ──────────┐
                     ├──> lumos-core (db, metadata, indexer, scan, watcher, clipboard, embeddings, search)
apps/lumos-app ──────┘
   │
   ├── src-tauri/lib.rs  ──> lumos-core   (via Tauri commands)
   ├── src-tauri/lib.rs  ──> Win32 directly (foreground_process_name — duplicated in clipboard_watcher.rs)
   ├── clipboard_watcher.rs ──> lumos-core::clipboard, lumos-core::db  (opens its own Connection)
   └── dist/app.js  ──> Tauri IPC only (invoke/listen) — correctly never touches lumos-core directly
```

Inside `lumos-core`, the real (undocumented) dependency graph is:

```
indexer   ──> metadata (resolve_project_for_path)
indexer   ──> embeddings (store, inline, best-effort)
clipboard ──> embeddings (store, inline, best-effort)
search    ──> clipboard::ClipboardResult   (concrete type, imported directly)
search    ──> files (ad hoc row-mapping, no shared type — see models.rs being dead)
search    ──> embeddings (search(), brute-force cosine)
scan      ──> indexer
watcher   ──> indexer
```

There is **no module that sits between `search` and the concrete source types** — `search.rs` is simultaneously "the merge point" and "the thing that knows both concrete schemas." This is the single most consequential fact in this map: Search and Ingestion are not actually separated today, they are fused in one file.

### 1.3 Current data flow (both real pipelines, traced end to end)

**Files:**
```
FS event / WalkDir  →  indexer::index_file()
   → std::fs::metadata (stat)
   → hash_file() (SHA-256, skip >50MB)
   → metadata::resolve_project_for_path()  [canonicalize() + full table scan, every call]
   → INSERT/UPDATE files  (autocommit, one txn per file)
   → SELECT id FROM files WHERE path=?  [redundant re-query, ON CONFLICT case]
   → embeddings::store("file", id, filename)  [best-effort, errors swallowed]
→ search::search() (raw FTS) or search::search_all() (FTS+semantic)
→ Tauri command search_files (holds shared Mutex<Connection> for entire call)
→ app.js renderResults
```

**Clipboard:**
```
GetClipboardSequenceNumber poll (500ms)  →  clipboard_watcher::spawn()
   → foreground_app_name() [Win32, duplicated]
   → clipboard::record_text()
       → dedup vs last row
       → classify() [18-branch if-chain]
       → build_search_extra() [+ decode_jwt_payload if JWT]
       → INSERT clipboard_entries (+ FTS trigger)
       → embeddings::store("clipboard", id, text)  [best-effort]
→ search::search_all()
→ Tauri command
→ app.js renderResults
```

**Screenshots / Browser / Sessions / Email:** no pipeline exists. Zero code.

### 1.4 What already (accidentally) matches the invariants

Worth stating so Phase 1-7 doesn't waste effort re-solving what's already correct:

- `indexer::index_file()` really is a single entry point for representing a file, regardless of how Lumos learned about it (scan vs watcher) — this is the §2 pipeline-unification instinct, just not yet formalized as named stages or extended to other sources.
- `embeddings::is_available()` graceful-degradation pattern (§8) is real and correctly used everywhere it matters.
- SQL parameterization is consistent — no injection surface, unrelated to this alignment pass but relevant to "safe to refactor without introducing new classes of bugs."
- UI (`app.js`) genuinely contains no business logic today — it renders `SearchHit` variants and calls commands. §6.1 already holds, by accident of small scope rather than by design, but it holds.

---

## 2. Violations against Architecture Invariants

Ordered by severity. "Where" cites exact files; "severity" reflects blast radius if left unaddressed while Screenshots/Browser/Sessions get built, not blast radius today.

### 🔴 Blocking — must be resolved before or during the phase that touches this area

**V1 — No Object model exists (§1).**
`files` and `clipboard_entries` are independent tables with independently-named equivalents of "created_at" (`created_at`/`modified_at` for files, `created_at` for clipboard), no shared identity scheme beyond what `embeddings` incidentally imposes via `(object_type, object_id)`. There is no `Object` envelope type anywhere in Rust code.
**Where:** `crates/lumos-core/src/db.rs` (schema), absence of any `Object`/`SearchObject` type in the whole crate.
**Severity: blocking for Phase 1.** Every later phase (Search genericity, plugin sources) depends on this existing first. This is correctly first in your phase ordering.

**V2 — Search is fused with concrete source types (§4.1, §6.5).**
`search.rs` imports `clipboard::ClipboardResult` directly and does ad hoc row-mapping for files (no shared type, since `models::FileRecord` is dead code, unused). `search_all()` contains explicit per-source branches and builds `SearchHit::File`/`SearchHit::Clipboard` by hand.
**Where:** `crates/lumos-core/src/search.rs` — the entire `search_all()` function and `SearchHit` enum.
**Severity: blocking for Phase 4.** This is the concrete violation that makes adding Screenshots today mean editing `search.rs`, which is exactly what §4.1/§7.1 forbid.

**V3 — Search output is concatenated blocks, not merged+ranked (§4.3, §4.4).**
`search_all()` produces FTS-files, then FTS-clipboard, then semantic-files, then semantic-clipboard, in that fixed order, with per-block truncation by remaining budget. There is no unified score; ordering between blocks is an accident of source, not relevance.
**Where:** `crates/lumos-core/src/search.rs`, `search_all()`.
**Severity: blocking for Phase 4** — this is Phase 4's actual deliverable, not a side effect.

**V4 — No storage boundary/trait; `rusqlite::Connection` passed raw everywhere (§6.3, §6.5).**
Every pipeline-stage-shaped function (`index_file`, `record_text`, `search`, `search_all`, `store`, `missing`, `backfill_all`) takes `&Connection` directly. Domain logic and infrastructure type are the same function signature.
**Where:** pervasive — `indexer.rs`, `clipboard.rs`, `search.rs`, `embeddings.rs`, `scan.rs`, `watcher.rs`, `metadata.rs` all take `&Connection`.
**Severity: blocking for Phase 2**, which exists specifically to introduce this boundary. Not urgent for correctness today (SQLite is the only and permanent choice), but blocking in the sense that Phase 3/4/6 all get harder to do cleanly if attempted before this.

### 🟠 Significant — should be resolved in its dedicated phase, will actively hurt if skipped

**V5 — No unified ingestion pipeline; each source hand-rolls its own stage sequence (§2, §2.1, §2.4).**
`indexer::index_file()` and `clipboard::record_text()` each independently do their own version of Normalize→Classify→Embed→Store, with different function shapes, different error-handling conventions, and no shared trait. There is no `Watch`/`Parse` separation at all — `index_file()` fuses stat+hash+classify+store in one function; `record_text()` fuses classify+store+embed in one function.
**Where:** `crates/lumos-core/src/indexer.rs::index_file()`, `crates/lumos-core/src/clipboard.rs::record_text()`.
**Severity: significant, this is Phase 3's job.** Not fixing this before adding Screenshots means Screenshots invents a *third* different shape for the same seven stages, compounding the problem instead of extending a pattern.

**V6 — No `SearchProvider`/plugin registration; sources are hardcoded into Search and into app wiring (§3, §7).**
There is no registry of "active sources." Adding a source today means: add a module, add its table to `db.rs`, add branches to `search.rs`, add a Tauri command if needed, add a UI render branch. Every one of those is an edit to a shared file.
**Where:** `crates/lumos-core/src/search.rs` (branches), `apps/lumos-app/src-tauri/src/lib.rs` (command registration is manual per feature).
**Severity: significant, Phase 6's job.** Depends on V1 (Object model) and V2 (Search genericity) being resolved first — attempting plugin registration before those exist just adds indirection around the same fused logic.

**V7 — LLM boundary not yet applicable, but the existing "intent" parsing already sets the wrong precedent (§5).**
No LLM exists yet, so §5.1/§5.2/§5.4 aren't violated by omission. But `search::parse_filters()` is the *only* thing playing the role of "produce a Search Intent," and it's a regex extraction function returning ad hoc `(String, Filters)` tuples, not a named `SearchIntent` type. When the LLM Interpreter is eventually added, if it's wired to *replace* `parse_filters()` output 1:1 without a formal `SearchIntent` schema existing first, it will be wired directly against whatever `search_all()` happens to accept that day — coupling the future LLM boundary to Search's internals instead of to a stable schema.
**Where:** `crates/lumos-core/src/search.rs::parse_filters()`, the `Filters` struct in `clipboard.rs`.
**Severity: significant but not urgent** — no LLM code exists to violate the boundary yet. Flagged so Phase 5 defines `SearchIntent` as a real type *before* any LLM code is written, not after.

### 🟡 Minor — real, but low blast-radius; fix opportunistically, not as a dedicated phase

**V8 — Duplicated Win32 foreground-process lookup (§6.3 spirit — infrastructure code duplicated instead of shared behind one seam).**
**Where:** `apps/lumos-app/src-tauri/src/lib.rs::foreground_process_name()` vs `clipboard_watcher.rs::foreground_app_name()`.
**Severity: minor**, pure DRY issue, not an architecture-shape violation — both call sites are already correctly in "infrastructure," just not deduplicated.

**V9 — `models.rs` dead code (no rule violated, just clutter that will confuse the Object-model work in Phase 1).**
**Where:** `crates/lumos-core/src/models.rs`.
**Severity: minor**, but worth deleting *during* Phase 1 specifically, since Phase 1 introduces the real `Object` type and a stale, similarly-named `FileRecord` sitting nearby is a real source of confusion for whoever does that work (including a future AI assistant pattern-matching on "there's already a models.rs").

**V10 — Derived-data cleanup on delete is implemented per-source, not via a shared lifecycle hook (§10.2).**
`indexer::remove_file()` and `clipboard::purge_older_than()` each separately clean up their own `embeddings` rows. Correct today, but it's two independent implementations of the same rule, not one enforced in one place.
**Where:** `crates/lumos-core/src/indexer.rs::remove_file()`, `crates/lumos-core/src/clipboard.rs::purge_older_than()`.
**Severity: minor now**, becomes significant the moment a third source arrives without its own careful reimplementation of the same cleanup — worth folding into whatever Phase 1's `Object` lifecycle work produces.

### Explicitly not violations (checked against the invariants, found compliant)

- §6.1 (UI has no business logic) — holds. `app.js` renders and calls commands; no ranking/classification/scoring logic found in it.
- §6.2 (Core never depends on UI) — holds structurally; `lumos-core` has no dependency on Tauri or `apps/*`.
- §8 (graceful degradation) — holds for the one subsystem that exists (`embeddings::is_available()`); nothing to violate for OCR/LLM since they don't exist yet.
- §2.3 (only Watch touches OS APIs directly) — holds inside `lumos-core` (`watcher.rs`, `clipboard.rs`'s dedup-only logic doesn't touch Win32; the actual clipboard read happens via `clipboard-win` at the watcher boundary). The one leak is in `apps/lumos-app` (V8), which is app-layer wiring, not a pipeline stage — flagged as V8, not as a §2.3 violation, since Tauri's `lib.rs` isn't a pipeline stage to begin with.

---

## 3. Recommended implementation order

Your proposed phase list (0-7) is already correctly sequenced by dependency, not by convenience. Confirming the dependency reasoning explicitly, plus calling out the one ordering risk to watch for:

```
Phase 0  Architecture alignment           (this document — no code)
Phase 1  Object Model                     unblocks: 2, 4, 6
Phase 2  Storage boundary                 unblocks: 3, 4 (cleanly); technically independent of 1,
                                           but doing it after Object Model exists means the storage
                                           trait can be shaped around Object from day one instead of
                                           being shaped around today's ad hoc row types and then
                                           reshaped again in Phase 4.
Phase 3  Unified ingestion pipeline       depends on: 1 (Object), 2 (storage trait) —
                                           rewriting index_file()/record_text() into named stages
                                           only makes sense once there's a real Object to normalize
                                           into and a storage seam to Store through.
Phase 4  Generic Search                   depends on: 1, 2, 3 — SearchObject/SearchProvider need
                                           Object (1) to be built from, a storage boundary (2) to
                                           query through, and a real pipeline (3) so "Index" is a
                                           defined stage each source already implements consistently.
Phase 5  LLM boundary                     depends on: 4 — SearchIntent as a real type only matters
                                           once Search actually consumes a typed Intent instead of
                                           an ad hoc (String, Filters) tuple; defining it before
                                           Phase 4 lands means defining it twice.
Phase 6  Plugin architecture              depends on: 1, 3, 4 — a plugin registry only pays off once
                                           there's a uniform pipeline shape (3) and a Search seam (4)
                                           to register into. Doing this first, as V6 notes, would just
                                           wrap today's fused logic in indirection.
Phase 7  Cleanup + invariants check       depends on: all of the above — re-audit against this same
                                           document, close V8/V9/V10, confirm §12 of the invariants
                                           doc ("known deviations") is now empty or intentionally
                                           re-scoped.
```

**The one ordering risk:** Phase 2 (Storage boundary) and Phase 1 (Object Model) touch overlapping code — every function that currently takes `&Connection` is a candidate edit site for *both* phases. Doing Phase 2 as "wrap `Connection` in a trait, keep today's row shapes" and then Phase 1 as "introduce `Object`, reshape those same functions again" means touching `indexer.rs`/`clipboard.rs`/`search.rs` twice for logically related reasons. Recommend explicitly within Phase 1's own scope: land the `Object` type *and* the minimal storage trait it needs together, even though the trait is nominally "Phase 2's" deliverable — then Phase 2 proper is "extend that trait to cover everything else (search queries, migrations, embeddings storage) that Phase 1 didn't need to touch," rather than two independent passes over the same call sites.

**What "safe migration, no big-bang" concretely means for Phase 1 specifically** (since it's next): the `Object` envelope should be introduced *alongside* the existing `files`/`clipboard_entries` tables, not as a replacement migration in the same phase. Concretely — add the shared identity/metadata concept as a thin layer that today's two sources populate in addition to their existing columns (or a view derived from them), verify Search and the pipeline still work against real data, and only fold `files`/`clipboard_entries`'s own metadata columns into the shared shape once Phase 4 (Search) is actually reading through the new abstraction end-to-end. Collapsing schema and abstraction into one migration risks exactly the "confidently wrong" failure mode this whole audit exists to avoid — better to have the abstraction and the legacy shape coexist for one phase than to get the target schema wrong on the first attempt with no working system to compare against.

No code has been changed in this pass. Phase 1 (Object Model) is the recommended next step.
