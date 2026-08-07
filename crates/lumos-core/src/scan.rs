use crate::indexer::index_file;
use crate::storage::{EmbeddingStore, ObjectStore, ProjectStore};
use anyhow::Result;
use std::path::Path;
use walkdir::WalkDir;

/// Directories skipped during a scan — dependency/build trees. Indexing
/// these wholesale would turn "scan my project" into "hash 200k files in
/// node_modules", with zero value for the memory-search use case.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    "bin",
    "obj",
];

#[derive(Debug, Default)]
pub struct ScanStats {
    pub indexed: u64,
    pub errors: u64,
}

pub fn scan_path<S>(store: &S, root: &Path) -> Result<ScanStats>
where
    S: ObjectStore + ProjectStore + EmbeddingStore,
{
    let mut stats = ScanStats::default();

    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        if e.file_type().is_dir() {
            let name = e.file_name().to_string_lossy();
            return !SKIP_DIRS.contains(&name.as_ref());
        }
        true
    });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        match index_file(store, entry.path()) {
            Ok(()) => stats.indexed += 1,
            Err(_) => stats.errors += 1,
        }
    }

    Ok(stats)
}
