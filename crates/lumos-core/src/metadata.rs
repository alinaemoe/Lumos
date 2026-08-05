use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Marker files checked in a project root to guess the kind of repo it is.
/// Order matters: more specific markers are checked before generic ones.
const REPO_TYPE_MARKERS: &[(&str, &str)] = &[
    ("Dockerfile", "Dockerized Project"),
    ("docker-compose.yml", "Dockerized Project"),
    ("Cargo.toml", "Rust Project"),
    ("package.json", "Node/JS Project"),
    ("*.csproj", ".NET Project"),
    ("*.sln", ".NET Project"),
];

/// Inspects a directory for well-known marker files and returns a human
/// readable guess of what kind of project lives there. Returns `None` if
/// nothing recognizable was found — the caller should not treat that as an
/// error, just as "unclassified".
pub fn detect_repo_type(root: &Path) -> Option<String> {
    for (marker, label) in REPO_TYPE_MARKERS {
        if let Some(pattern) = marker.strip_prefix('*') {
            // crude glob: match by file extension suffix in the top-level dir only
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().ends_with(pattern) {
                        return Some((*label).to_string());
                    }
                }
            }
        } else if root.join(marker).exists() {
            return Some((*label).to_string());
        }
    }
    None
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Creates the project if it doesn't exist yet, returns its id.
pub fn get_or_create_project(conn: &Connection, name: &str) -> Result<i64> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM projects WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .optional()
        .context("looking up project by name")?;

    if let Some(id) = existing {
        return Ok(id);
    }

    conn.execute(
        "INSERT INTO projects (name, created_at) VALUES (?1, ?2)",
        rusqlite::params![name, now()],
    )
    .context("inserting project")?;
    Ok(conn.last_insert_rowid())
}

/// Canonicalizes a path for stable comparisons. On Windows,
/// `Path::canonicalize` returns a `\\?\`-prefixed verbatim path, which
/// plain paths (e.g. from `walkdir`) never have — left alone, that prefix
/// mismatch means every prefix comparison against a canonicalized path
/// silently fails. Stripping it here keeps both sides comparable.
fn normalize_path(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let s = canonical.to_string_lossy().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Registers a folder as belonging to a project. Auto-detects repo type
/// from marker files unless one is already known. This is the one place a
/// human-provided project name meets the filesystem — everything downstream
/// (files, sessions, clipboard) resolves its project through this table.
pub fn register_project_path(conn: &Connection, project_id: i64, path: &Path) -> Result<()> {
    let path_str = normalize_path(path);
    let repo_type = detect_repo_type(path);

    conn.execute(
        "INSERT INTO project_paths (project_id, path, repo_type)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET project_id = excluded.project_id, repo_type = excluded.repo_type",
        rusqlite::params![project_id, path_str, repo_type],
    )
    .context("registering project path")?;
    Ok(())
}

/// Finds which project owns a given file path by matching against all
/// registered project_paths and picking the longest (most specific) prefix.
pub fn resolve_project_for_path(conn: &Connection, file_path: &Path) -> Result<Option<i64>> {
    let file_path_str = normalize_path(file_path);

    let mut stmt = conn.prepare("SELECT project_id, path FROM project_paths")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut best: Option<(i64, usize)> = None;
    for (project_id, root) in rows {
        if file_path_str.starts_with(&root) {
            let len = root.len();
            if best.map(|(_, best_len)| len > best_len).unwrap_or(true) {
                best = Some((project_id, len));
            }
        }
    }
    Ok(best.map(|(id, _)| id))
}
