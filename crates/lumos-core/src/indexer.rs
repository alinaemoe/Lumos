use crate::metadata::resolve_project_for_path;
use crate::object::{FilePayload, ObjectMetadata, ObjectPayload, ObjectType};
use crate::pipeline::{ingest, Ingestable};
use crate::storage::{EmbeddingStore, NewObject, ObjectStore, ProjectStore};
use anyhow::Result;
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
///
/// The signature states its needs: it writes a file row, resolves a
/// project, and stores an embedding. Nothing about SQLite appears in it.
pub fn index_file<S>(store: &S, path: &Path) -> Result<()>
where
    S: ObjectStore + ProjectStore + EmbeddingStore,
{
    // --- Parse: turn the raw filesystem event into file data. ---
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return remove_file(store, path), // vanished between event and read
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

    // --- Classify: which project this file belongs to. Project
    // association is this source's only semantic classification — there
    // is no file-type taxonomy today beyond the raw extension. ---
    let project_id = resolve_project_for_path(store, path)?;

    // --- Normalize: shape parsed data into the canonical Object envelope.
    // Past this point nothing below knows this came from a filesystem
    // event. ---
    let object = NewObject::new(
        ObjectMetadata {
            created_at: Some(to_epoch_secs(meta.created())),
            updated_at: Some(to_epoch_secs(meta.modified())),
            project_id,
            source: None,
        },
        ObjectPayload::File(FilePayload {
            path: path.to_string_lossy().to_string(),
            filename: filename.clone(),
            extension,
            size: size as i64,
            hash,
            // Write-side objects don't carry the project's display name —
            // that's a read-side join. See the adapter note in object.rs.
            project_name: None,
        }),
    );

    // --- Embed + Store: shared with every other source, see pipeline.rs. ---
    ingest(store, Ingestable::new(object, Some(filename)))?;

    Ok(())
}

/// Generic in shape: resolve a natural key to an identity, then delete
/// that identity. Nothing here is file-specific except which
/// `ObjectType` is named — deleting a screenshot or an email will be the
/// same two calls. Derived data (embeddings) is removed inside
/// `ObjectStore::delete`, so no caller has to remember §10.2.
pub fn remove_file<S>(store: &S, path: &Path) -> Result<()>
where
    S: ObjectStore,
{
    let path_str = path.to_string_lossy().to_string();
    if let Some(id) = store.resolve(ObjectType::File, &path_str)? {
        store.delete(id)?;
    }
    Ok(())
}

pub fn rename_file<S>(store: &S, from: &Path, to: &Path) -> Result<()>
where
    S: ObjectStore + ProjectStore + EmbeddingStore,
{
    remove_file(store, from)?;
    index_file(store, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_sqlite::SqliteStore;
    use std::io::Write;

    /// Full stack, no mocks: real filesystem file, real in-memory SQLite
    /// schema (triggers included), real `index_file`, real `search::search`
    /// reading through FTS. This is the "raw file event → Object → store →
    /// search/index remains functional" acceptance test.
    #[test]
    fn file_event_flows_through_to_search() {
        let dir = std::env::temp_dir().join(format!("lumos-pipeline-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("hydration_notes.rs");
        write!(std::fs::File::create(&file_path).unwrap(), "fn main() {{}}").unwrap();

        let conn = crate::db::open_in_memory().unwrap();
        let store = SqliteStore::new(&conn);

        index_file(&store, &file_path).expect("indexing must succeed");

        let hits = crate::search::search(&store, "hydration_notes", None, 10).unwrap();
        assert_eq!(hits.len(), 1, "the file must be findable through FTS after indexing");
        assert_eq!(hits[0].filename, "hydration_notes.rs");

        // Cascade delete: remove_file -> ObjectStore::delete -> row + FTS gone.
        remove_file(&store, &file_path).unwrap();
        let hits_after = crate::search::search(&store, "hydration_notes", None, 10).unwrap();
        assert!(hits_after.is_empty(), "deleted files must not remain searchable");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reindexing_the_same_path_updates_rather_than_duplicates() {
        let dir = std::env::temp_dir().join(format!("lumos-pipeline-reindex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("a.txt");
        std::fs::write(&file_path, "v1").unwrap();

        let conn = crate::db::open_in_memory().unwrap();
        let store = SqliteStore::new(&conn);

        index_file(&store, &file_path).unwrap();
        std::fs::write(&file_path, "v2, now longer").unwrap();
        index_file(&store, &file_path).unwrap();

        let n: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "re-indexing a path must update, not duplicate");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
