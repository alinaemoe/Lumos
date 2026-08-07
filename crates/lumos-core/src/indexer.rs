use crate::metadata::resolve_project_for_path;
use crate::object::{ObjectId, ObjectType};
use crate::storage::{EmbeddingStore, FileStore, FileUpsert, ProjectStore};
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
    S: FileStore + ProjectStore + EmbeddingStore,
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

    let record = FileUpsert {
        path: path.to_string_lossy().to_string(),
        filename: filename.clone(),
        extension,
        size: size as i64,
        hash,
        project_id,
        created_at: to_epoch_secs(meta.created()),
        modified_at: to_epoch_secs(meta.modified()),
    };
    let file_id = store.upsert_file(&record)?;

    // Best-effort — a filename embedding is small and quick (one short
    // sequence through a small CPU model), and `store` itself no-ops
    // silently if the model never loaded. Failures here shouldn't fail
    // indexing over what's genuinely a search-quality nicety.
    let _ = crate::embeddings::store(store, ObjectId::new(ObjectType::File, file_id), &filename);

    Ok(())
}

pub fn remove_file<S>(store: &S, path: &Path) -> Result<()>
where
    S: FileStore + EmbeddingStore,
{
    let path_str = path.to_string_lossy().to_string();
    // Derived data dies with the object it was derived from —
    // `ARCHITECTURE_INVARIANTS.md` §10.2.
    if let Some(id) = store.remove_file_by_path(&path_str)? {
        let _ = store.remove_vector(ObjectId::new(ObjectType::File, id));
    }
    Ok(())
}

pub fn rename_file<S>(store: &S, from: &Path, to: &Path) -> Result<()>
where
    S: FileStore + ProjectStore + EmbeddingStore,
{
    remove_file(store, from)?;
    index_file(store, to)
}
