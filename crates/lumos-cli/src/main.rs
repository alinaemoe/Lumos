use anyhow::Result;
use clap::{Parser, Subcommand};
use lumos_core::storage_sqlite::SqliteStore;
use lumos_core::{clipboard, db, embeddings, metadata, scan, search, watcher};
use rusqlite::Connection;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "lumos", about = "Lumos — personal computer memory system (core CLI)")]
struct Cli {
    /// Override the default database location (%LOCALAPPDATA%\Lumos\lumos.db)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage named projects and the folders that belong to them.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Walk a directory and index every file under it into the database.
    Scan {
        path: PathBuf,
    },
    /// Watch registered (or given) directories live and index changes as they happen.
    Watch {
        /// Specific paths to watch. If omitted, watches every registered project path.
        paths: Vec<PathBuf>,
    },
    /// Smoke-test the FTS5 index with a raw filename/path query.
    Search {
        query: String,
    },
    /// The real unified search (FTS5 + semantic, files + clipboard) — same
    /// call the overlay UI makes.
    SearchAll {
        query: String,
    },
    /// Clipboard history — mainly for testing the monitor without a live
    /// clipboard listener attached.
    Clipboard {
        #[command(subcommand)]
        action: ClipboardAction,
    },
    /// Semantic search backfill — downloads the embedding model on first
    /// run (needs network once), then embeds any file/clipboard row that
    /// doesn't have a vector yet.
    Embed,
}

#[derive(Subcommand)]
enum ClipboardAction {
    /// Feed a piece of text through the same path a real clipboard copy
    /// would take (classification + dedup against the last entry).
    Record { text: String },
    /// Most recent clipboard entries, newest first.
    List {
        #[arg(long, default_value = "20")]
        limit: i64,
    },
    /// FTS5 search over clipboard history. Supports `type:xxx`, `today`,
    /// `yesterday`, `week` filter tokens mixed in with free text.
    Search { query: String },
    /// Pin an entry so retention cleanup never deletes it.
    Pin { id: i64 },
    /// Unpin an entry.
    Unpin { id: i64 },
    /// Delete unpinned entries older than N days.
    Purge { days: i64 },
}

#[derive(Subcommand)]
enum ProjectAction {
    /// Register a folder as belonging to a project (creates the project if new).
    Add { name: String, path: PathBuf },
    /// List known projects and their registered paths.
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(db::default_db_path);
    let conn = db::open(&db_path)?;

    match cli.command {
        Command::Project { action } => run_project(&conn, action)?,
        Command::Scan { path } => run_scan(&conn, &path)?,
        Command::Watch { paths } => run_watch(&conn, paths)?,
        Command::Search { query } => run_search(&conn, &query)?,
        Command::SearchAll { query } => run_search_all(&conn, &query)?,
        Command::Clipboard { action } => run_clipboard(&conn, action)?,
        Command::Embed => run_embed(&conn)?,
    }

    Ok(())
}

fn run_project(conn: &Connection, action: ProjectAction) -> Result<()> {
    match action {
        ProjectAction::Add { name, path } => {
            let store = SqliteStore::new(conn);
            let project_id = metadata::get_or_create_project(&store, &name)?;
            metadata::register_project_path(&store, project_id, &path)?;
            println!("registered {:?} under project '{name}' (id {project_id})", path);
        }
        ProjectAction::List => {
            let mut stmt = conn.prepare(
                "SELECT p.name, pp.path, pp.repo_type
                 FROM projects p
                 JOIN project_paths pp ON pp.project_id = p.id
                 ORDER BY p.name",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (name, path, repo_type) = row?;
                println!("{name:20} {path:50} {}", repo_type.unwrap_or_else(|| "-".into()));
            }
        }
    }
    Ok(())
}

fn run_scan(conn: &Connection, path: &PathBuf) -> Result<()> {
    println!("scanning {:?}...", path);
    let stats = scan::scan_path(&SqliteStore::new(conn), path)?;
    println!("indexed {} file(s), {} error(s)", stats.indexed, stats.errors);
    Ok(())
}

fn run_watch(conn: &Connection, paths: Vec<PathBuf>) -> Result<()> {
    let roots = if paths.is_empty() {
        let mut stmt = conn.prepare("SELECT path FROM project_paths")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.filter_map(|r| r.ok()).map(PathBuf::from).collect()
    } else {
        paths
    };

    if roots.is_empty() {
        println!("no paths to watch — register a project first (`lumos project add <name> <path>`) or pass paths explicitly");
        return Ok(());
    }

    watcher::watch_and_index(&SqliteStore::new(conn), &roots)
}

fn run_search(conn: &Connection, query: &str) -> Result<()> {
    let results = search::search(conn, query, None, 20)?;
    if results.is_empty() {
        println!("no matches for {query:?}");
        return Ok(());
    }
    for r in results {
        let project = r.project_name.as_deref().unwrap_or("-");
        println!("{}  ({} bytes, project={})", r.path, r.size, project);
    }
    Ok(())
}

fn run_search_all(conn: &Connection, query: &str) -> Result<()> {
    let hits = search::search_all(conn, query, 20)?;
    if hits.is_empty() {
        println!("no matches for {query:?}");
        return Ok(());
    }
    for hit in hits {
        match hit {
            search::SearchHit::File(f) => println!("[file] {} (project={})", f.path, f.project_name.unwrap_or_else(|| "-".into())),
            search::SearchHit::Clipboard(c) => {
                let preview: String = c.text_content.chars().take(80).collect();
                println!("[clip:{}] {}", c.detected_type, preview.replace('\n', "⏎"));
            }
        }
    }
    Ok(())
}

fn print_clip(c: &clipboard::ClipboardResult) {
    let preview: String = c.text_content.chars().take(80).collect();
    let preview = preview.replace('\n', "⏎");
    let pin = if c.pinned { "📌" } else { "  " };
    let source = c.source_app.as_deref().unwrap_or("-");
    println!("{pin}[{}] #{} {} ({source}): {}", c.created_at, c.id, c.detected_type, preview);
}

fn run_clipboard(conn: &Connection, action: ClipboardAction) -> Result<()> {
    match action {
        ClipboardAction::Record { text } => match clipboard::record_text(&SqliteStore::new(conn), &text, None)? {
            Some(id) => println!("recorded #{id} (type: {})", clipboard::classify(&text)),
            None => println!("skipped (empty or duplicate of last entry)"),
        },
        ClipboardAction::List { limit } => {
            let entries = clipboard::recent(conn, limit)?;
            if entries.is_empty() {
                println!("no clipboard history yet");
            }
            for c in entries {
                print_clip(&c);
            }
        }
        ClipboardAction::Search { query } => {
            let (text, filters) = search::parse_filters(&query);
            let entries = clipboard::search(conn, &text, &filters, 20)?;
            if entries.is_empty() {
                println!("no matches for {query:?}");
            }
            for c in entries {
                print_clip(&c);
            }
        }
        ClipboardAction::Pin { id } => {
            clipboard::set_pinned(conn, id, true)?;
            println!("pinned #{id}");
        }
        ClipboardAction::Unpin { id } => {
            clipboard::set_pinned(conn, id, false)?;
            println!("unpinned #{id}");
        }
        ClipboardAction::Purge { days } => {
            let removed = clipboard::purge_older_than(conn, days)?;
            println!("removed {removed} entr{} older than {days} day(s)", if removed == 1 { "y" } else { "ies" });
        }
    }
    Ok(())
}

fn run_embed(conn: &Connection) -> Result<()> {
    if !embeddings::is_available() {
        println!("embedding model unavailable (no network on first run, or it failed to load) — semantic search stays off, FTS keeps working");
        return Ok(());
    }
    let (files, clips) = embeddings::backfill_all(&SqliteStore::new(conn))?;
    println!("embedded {files} file(s), {clips} clipboard entr{}", if clips == 1 { "y" } else { "ies" });
    Ok(())
}
