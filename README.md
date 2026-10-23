# Lumos

**A local memory for your computer: find what you did, not just where you saved it.**

Lumos watches your files and clipboard on your own machine and lets you search them the way you
remember them - by meaning, time and kind - from a Tauri overlay opened with a **double-tap of Ctrl**.
Everything runs locally; there is no cloud, and the system works fully **without any LLM**.

> You don't remember the file name. You remember "that Azure certificate error from March" and the
> command you copied from Stack Overflow. Lumos is built for that.

## What works today

- **Files.** Directory scan and live file watching, content and path indexed in SQLite FTS5, folders
  grouped into named projects.
- **Clipboard history.** A clipboard monitor with classification of what was copied (JWT, connection
  strings, secrets, SQL, JSON, stack traces, shell commands...), so you can search "that SQL query" or
  "the token I copied yesterday" instead of scrolling a list.
- **Hybrid search.** Full-text (FTS5) and **semantic** search (`multilingual-e5-small`, quantized ONNX
  via `ort`) fused with Reciprocal Rank Fusion, across files and clipboard in one query. Works for
  Russian and English text.
- **Overlay UI.** Tauri desktop app: global double-tap-Ctrl detector, autostart, instant search window.
- **Optional natural-language filters.** A local Ollama model can turn "errors from last week" into a
  structured filter; if it isn't running, a deterministic parser (`type:`, `today`, `week`) takes over.
  The LLM only fills a fixed JSON schema - it never writes SQL and never answers questions.
- **CLI.** `lumos project`, `scan`, `watch`, `search`, `search-all`, `clipboard`, and a semantic
  backfill command that downloads the embedding model on first run.

## Design principles

- **Local-first and private.** One SQLite file, no network except the one-time model download.
- **Deterministic core, optional AI.** Classification, sessions and search are rules plus embeddings;
  an LLM is an opt-in translation layer, never the foundation.
- **Honest scope.** The docs say explicitly what exists and what is only designed. *Not built yet:*
  screenshots + OCR/CLIP, browser history, activity sessions, email.
- **Extensible by construction.** Everything searchable is an *Object*; ingestion is plugin-shaped and
  search is generic over object types ([`ARCHITECTURE_INVARIANTS.md`](ARCHITECTURE_INVARIANTS.md)).

## Tech

Rust workspace (`lumos-core`, `lumos-cli`, `lumos-app`) - `rusqlite`
(bundled SQLite + FTS5), `notify` file watching, ONNX Runtime (`ort`) + WordPiece tokenizer for
embeddings, `clap` CLI, Tauri 2 desktop shell. 150+ unit tests in the core crate. Windows is the
primary target (clipboard and hotkey integration).

## Run it

```bash
cargo test                              # core tests
cargo run -p lumos-cli -- --help        # CLI (db: %LOCALAPPDATA%\Lumos\lumos.db)
cargo run -p lumos-cli -- scan <dir>    # index a folder
cargo run -p lumos-cli -- embed         # build semantic vectors (downloads the model once)
cargo run -p lumos-cli -- search-all "azure certificate"
```

The overlay app lives in `apps/lumos-app` (Tauri 2; `cargo tauri dev` from `apps/lumos-app/src-tauri`).

## Read more

| Doc | What |
|---|---|
| [`LUMOS_DESIGN.md`](LUMOS_DESIGN.md) | Technical design: problem, architecture, stack, data model |
| [`LUMOS_STORY.md`](LUMOS_STORY.md) | The project story: decisions, mistakes and the current direction (Russian) |
| [`ARCHITECTURE_INVARIANTS.md`](ARCHITECTURE_INVARIANTS.md) | Rules the architecture must keep: object model, ingestion, LLM boundary, graceful degradation |
| [`LUMOS_CODE_REVIEW.md`](LUMOS_CODE_REVIEW.md) | A review of the codebase and its follow-ups |

Status: v0.1, a personal tool built in the open.
