use crate::metadata::resolve_project_for_path;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Files larger than this are indexed without a content hash — hashing a
/// multi-gigabyte video or VM image on every watcher event would stall the
/// indexer for no real benefit (we still get path/size/mtime for those).
const MAX_HASH_BYTES: u64 = 50 * 1024 * 1024;

fn hash_file(path: &Path, size: u64) -> Option<String> {
    if size > MAX_HASH_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn to_epoch_secs(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Indexes (or re-indexes) a single file: stats it, hashes it, resolves its
/// project via the registered project_paths, and upserts the `files` row.
/// This is the single entry point both the bulk scanner and the live
/// watcher funnel through, so a file is always represented the same way
/// regardless of how Lumos learned about it.
pub fn index_file(conn: &Connection, path: &Path) -> Result<()> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return remove_file(conn, path), // vanished between event and read
    };
    if !meta.is_file() {
        return Ok(());
    }

    let size = meta.len();
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase());
    let hash = hash_file(path, size);
    let project_id = resolve_project_for_path(conn, path)?;
    let created_at = to_epoch_secs(meta.created());
    let modified_at = to_epoch_secs(meta.modified());
    let path_str = path.to_string_lossy().to_string();

    conn.execute(
        "INSERT INTO files (path, filename, extension, size, hash, project_id, created_at, modified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(path) DO UPDATE SET
            filename = excluded.filename,
            extension = excluded.extension,
            size = excluded.size,
            hash = excluded.hash,
            project_id = excluded.project_id,
            modified_at = excluded.modified_at",
        rusqlite::params![path_str, filename, extension, size as i64, hash, project_id, created_at, modified_at],
    )
    .with_context(|| format!("upserting file {:?}", path))?;

    // Best-effort — a filename embedding is small and quick (one short
    // sequence through a small CPU model), and `store` itself no-ops
    // silently if the model never loaded. Failures here shouldn't fail
    // indexing over what's genuinely a search-quality nicety.
    if let Ok(file_id) = conn.query_row("SELECT id FROM files WHERE path = ?1", [&path_str], |r| r.get::<_, i64>(0)) {
        let _ = crate::embeddings::store(conn, "file", file_id, &filename);
    }

    Ok(())
}

pub fn remove_file(conn: &Connection, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();
    let id: Option<i64> = conn
        .query_row("SELECT id FROM files WHERE path = ?1", [&path_str], |r| r.get(0))
        .optional()
        .context("looking up file id before delete")?;
    conn.execute("DELETE FROM files WHERE path = ?1", [&path_str])
        .context("deleting file row")?;
    if let Some(id) = id {
        let _ = conn.execute("DELETE FROM embeddings WHERE object_type = 'file' AND object_id = ?1", [id]);
    }
    Ok(())
}

pub fn rename_file(conn: &Connection, from: &Path, to: &Path) -> Result<()> {
    remove_file(conn, from)?;
    index_file(conn, to)
}
