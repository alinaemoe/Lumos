use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS project_paths (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path TEXT NOT NULL UNIQUE,
    git_remote TEXT,
    repo_type TEXT
);
CREATE INDEX IF NOT EXISTS idx_project_paths_path ON project_paths(path);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    filename TEXT NOT NULL,
    extension TEXT,
    size INTEGER,
    hash TEXT,
    file_type TEXT,
    project_id INTEGER REFERENCES projects(id),
    created_at INTEGER,
    modified_at INTEGER,
    last_opened_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_files_project ON files(project_id);

CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
    filename, path, content='files', content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
    INSERT INTO files_fts(rowid, filename, path) VALUES (new.id, new.filename, new.path);
END;

CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, filename, path) VALUES('delete', old.id, old.filename, old.path);
END;

CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, filename, path) VALUES('delete', old.id, old.filename, old.path);
    INSERT INTO files_fts(rowid, filename, path) VALUES (new.id, new.filename, new.path);
END;

CREATE TABLE IF NOT EXISTS clipboard_entries (
    id INTEGER PRIMARY KEY,
    content_type TEXT NOT NULL DEFAULT 'text',
    text_content TEXT,
    detected_type TEXT,
    search_extra TEXT,
    source_app TEXT,
    pinned INTEGER NOT NULL DEFAULT 0,
    project_id INTEGER REFERENCES projects(id),
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_clipboard_created ON clipboard_entries(created_at);

CREATE VIRTUAL TABLE IF NOT EXISTS clipboard_fts USING fts5(
    text_content, search_extra, content='clipboard_entries', content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS clipboard_ai AFTER INSERT ON clipboard_entries BEGIN
    INSERT INTO clipboard_fts(rowid, text_content, search_extra) VALUES (new.id, new.text_content, new.search_extra);
END;

CREATE TRIGGER IF NOT EXISTS clipboard_ad AFTER DELETE ON clipboard_entries BEGIN
    INSERT INTO clipboard_fts(clipboard_fts, rowid, text_content, search_extra) VALUES('delete', old.id, old.text_content, old.search_extra);
END;

CREATE TRIGGER IF NOT EXISTS clipboard_au AFTER UPDATE ON clipboard_entries BEGIN
    INSERT INTO clipboard_fts(clipboard_fts, rowid, text_content, search_extra) VALUES('delete', old.id, old.text_content, old.search_extra);
    INSERT INTO clipboard_fts(rowid, text_content, search_extra) VALUES (new.id, new.text_content, new.search_extra);
END;

-- One row per (object_type, object_id) — 'file' -> files.id, 'clipboard' ->
-- clipboard_entries.id. No FTS table: these are looked up by brute-force
-- cosine scan in Rust, not matched via SQL text search. See embeddings.rs
-- for why sqlite-vec isn't used at this data scale.
CREATE TABLE IF NOT EXISTS embeddings (
    object_type TEXT NOT NULL,
    object_id INTEGER NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY (object_type, object_id)
);
"#;

/// `search_extra` (type name + synonyms + decoded JWT payload, etc.) was
/// added after `clipboard_entries`/`clipboard_fts` already shipped — a
/// plain `CREATE TABLE IF NOT EXISTS` is a no-op against an existing table,
/// so a database created before this column existed needs an explicit,
/// one-time upgrade rather than just rerunning SCHEMA.
fn migrate_clipboard_search_extra(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('clipboard_entries') WHERE name = 'search_extra'")?
        .exists([])?;
    if has_column {
        return Ok(());
    }

    conn.execute_batch(
        "ALTER TABLE clipboard_entries ADD COLUMN search_extra TEXT;
         DROP TRIGGER IF EXISTS clipboard_ai;
         DROP TRIGGER IF EXISTS clipboard_ad;
         DROP TRIGGER IF EXISTS clipboard_au;
         DROP TABLE IF EXISTS clipboard_fts;
         CREATE VIRTUAL TABLE clipboard_fts USING fts5(
             text_content, search_extra, content='clipboard_entries', content_rowid='id'
         );
         INSERT INTO clipboard_fts(rowid, text_content, search_extra)
             SELECT id, text_content, search_extra FROM clipboard_entries;
         CREATE TRIGGER clipboard_ai AFTER INSERT ON clipboard_entries BEGIN
             INSERT INTO clipboard_fts(rowid, text_content, search_extra) VALUES (new.id, new.text_content, new.search_extra);
         END;
         CREATE TRIGGER clipboard_ad AFTER DELETE ON clipboard_entries BEGIN
             INSERT INTO clipboard_fts(clipboard_fts, rowid, text_content, search_extra) VALUES('delete', old.id, old.text_content, old.search_extra);
         END;
         CREATE TRIGGER clipboard_au AFTER UPDATE ON clipboard_entries BEGIN
             INSERT INTO clipboard_fts(clipboard_fts, rowid, text_content, search_extra) VALUES('delete', old.id, old.text_content, old.search_extra);
             INSERT INTO clipboard_fts(rowid, text_content, search_extra) VALUES (new.id, new.text_content, new.search_extra);
         END;",
    )
    .context("migrating clipboard_entries to add search_extra")?;
    Ok(())
}

/// `source_app` and `pinned` were added alongside `search_extra` but need
/// no FTS rebuild — they're plain columns, not indexed content — so a
/// simple guarded `ALTER TABLE` per column is enough.
fn migrate_clipboard_columns(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        ("source_app", "ALTER TABLE clipboard_entries ADD COLUMN source_app TEXT"),
        (
            "pinned",
            "ALTER TABLE clipboard_entries ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        let has_column: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('clipboard_entries') WHERE name = ?1")?
            .exists([column])?;
        if !has_column {
            conn.execute(ddl, [])
                .with_context(|| format!("adding clipboard_entries.{column}"))?;
        }
    }
    Ok(())
}

pub fn open(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating db directory {:?}", parent))?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening database at {:?}", db_path))?;
    conn.execute_batch(SCHEMA).context("running schema migration")?;
    migrate_clipboard_search_extra(&conn)?;
    migrate_clipboard_columns(&conn)?;
    Ok(conn)
}

/// An empty database with the real schema, held in memory. For tests that
/// need to exercise storage behaviour without touching disk.
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("opening in-memory database")?;
    conn.execute_batch(SCHEMA).context("running schema migration")?;
    Ok(conn)
}

pub fn default_db_path() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Lumos").join("lumos.db")
}
