pub mod clipboard;
pub mod db;
pub mod embeddings;
pub mod indexer;
pub mod intent_schema;
pub mod interpreter;
pub mod metadata;
pub mod models;
pub mod object;
pub mod object_adapters;
pub mod pipeline;
pub mod scan;
pub mod search;
pub mod storage;
pub mod storage_sqlite;
pub mod watcher;

/// Unix seconds. Several modules had their own private copy of this; the
/// storage layer needed one too, so it lives here now rather than becoming
/// a fourth duplicate.
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
