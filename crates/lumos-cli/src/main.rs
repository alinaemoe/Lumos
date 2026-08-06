use anyhow::Result;
use clap::{Parser, Subcommand};
use lumos_core::{clipboard, db, metadata, scan, search, watcher};
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
    /// Clipboard history — mainly for testing the monitor without a live
    /// clipboard listener attached.
    Clipboard {
        #[command(subcommand)]
        action: ClipboardAction,
    },
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
    /// FTS5 search over clipboard history.
    Search { query: String },
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
        Command::Clipboard { action } => run_clipboard(&conn, action)?,
    }

    Ok(())
}

fn run_project(conn: &Connection, action: ProjectAction) -> Result<()> {
    match action {
        ProjectAction::Add { name, path } => {
            let project_id = metadata::get_or_create_project(conn, &name)?;
            metadata::register_project_path(conn, project_id, &path)?;
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
    let stats = scan::scan_path(conn, path)?;
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

    watcher::watch_and_index(conn, &roots)
}

fn run_search(conn: &Connection, query: &str) -> Result<()> {
    let results = search::search(conn, query, 20)?;
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

fn print_clip(c: &clipboard::ClipboardResult) {
    let preview: String = c.text_content.chars().take(80).collect();
    let preview = preview.replace('\n', "⏎");
    println!("[{}] #{} {}: {}", c.created_at, c.id, c.detected_type, preview);
}

fn run_clipboard(conn: &Connection, action: ClipboardAction) -> Result<()> {
    match action {
        ClipboardAction::Record { text } => match clipboard::record_text(conn, &text)? {
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
            let entries = clipboard::search(conn, &query, 20)?;
            if entries.is_empty() {
                println!("no matches for {query:?}");
            }
            for c in entries {
                print_clip(&c);
            }
        }
    }
    Ok(())
}
