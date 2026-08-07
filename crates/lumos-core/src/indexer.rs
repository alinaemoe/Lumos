use crate::metadata::resolve_project_for_path;
use crate::object::{FilePayload, ObjectMetadata, ObjectPayload, ObjectType};
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
    let project_id = resolve_project_for_path(store, path)?;

    // One object, one `save` — the pipeline never names a table or picks
    // between insert and update. Storage owns that.
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
    let id = store.save(&object)?;

    // Best-effort — a filename embedding is small and quick (one short
    // sequence through a small CPU model), and `store` itself no-ops
    // silently if the model never loaded. Failures here shouldn't fail
    // indexing over what's genuinely a search-quality nicety.
    let _ = crate::embeddings::store(store, id, &filename);

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
