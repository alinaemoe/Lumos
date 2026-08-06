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
    project_id INTEGER REFERENCES projects(id),
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_clipboard_created ON clipboard_entries(created_at);

CREATE VIRTUAL TABLE IF NOT EXISTS clipboard_fts USING fts5(
    text_content, content='clipboard_entries', content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS clipboard_ai AFTER INSERT ON clipboard_entries BEGIN
    INSERT INTO clipboard_fts(rowid, text_content) VALUES (new.id, new.text_content);
END;

CREATE TRIGGER IF NOT EXISTS clipboard_ad AFTER DELETE ON clipboard_entries BEGIN
    INSERT INTO clipboard_fts(clipboard_fts, rowid, text_content) VALUES('delete', old.id, old.text_content);
END;

CREATE TRIGGER IF NOT EXISTS clipboard_au AFTER UPDATE ON clipboard_entries BEGIN
    INSERT INTO clipboard_fts(clipboard_fts, rowid, text_content) VALUES('delete', old.id, old.text_content);
    INSERT INTO clipboard_fts(rowid, text_content) VALUES (new.id, new.text_content);
END;
"#;

pub fn open(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating db directory {:?}", parent))?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening database at {:?}", db_path))?;
    conn.execute_batch(SCHEMA).context("running schema migration")?;
    Ok(conn)
}

pub fn default_db_path() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Lumos").join("lumos.db")
}
