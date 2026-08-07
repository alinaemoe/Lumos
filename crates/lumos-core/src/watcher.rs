use crate::indexer;
use crate::storage::{EmbeddingStore, ObjectStore, ProjectStore};
use anyhow::{Context, Result};
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::channel;

/// Watches the given root directories for filesystem changes and feeds them
/// straight into the indexer, live. Runs forever (blocking) — intended to be
/// called from a long-running `lumos watch` process, not a one-shot CLI call.
///
/// On Windows this backs onto `ReadDirectoryChangesW` via the `notify` crate
/// rather than hand-rolling the WinAPI binding ourselves: same underlying
/// mechanism, far less surface area to get wrong solo.
pub fn watch_and_index<S>(store: &S, roots: &[PathBuf]) -> Result<()>
where
    S: ObjectStore + ProjectStore + EmbeddingStore,
{
    let (tx, rx) = channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;

    for root in roots {
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {:?}", root))?;
        println!("watching {:?}", root);
    }

    for res in rx {
        match res {
            Ok(event) => handle_event(store, &event),
            Err(e) => eprintln!("watch error: {e}"),
        }
    }

    Ok(())
}

fn handle_event<S>(store: &S, event: &Event)
where
    S: ObjectStore + ProjectStore + EmbeddingStore,
{
    match &event.kind {
        EventKind::Create(_) => {
            for p in &event.paths {
                if p.is_file() {
                    if let Err(e) = indexer::index_file(store, p) {
                        eprintln!("index error for {:?}: {e}", p);
                    }
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(_)) => {
            // Rename: notify reports [old_path, new_path] when the platform
            // backend can pair them (Windows can, via ReadDirectoryChangesW).
            if event.paths.len() == 2 {
                if let Err(e) = indexer::rename_file(store, &event.paths[0], &event.paths[1]) {
                    eprintln!("rename error {:?} -> {:?}: {e}", event.paths[0], event.paths[1]);
                }
            } else {
                for p in &event.paths {
                    let result = if p.exists() {
                        indexer::index_file(store, p)
                    } else {
                        indexer::remove_file(store, p)
                    };
                    if let Err(e) = result {
                        eprintln!("rename fallback error for {:?}: {e}", p);
                    }
                }
            }
        }
        EventKind::Modify(_) => {
            for p in &event.paths {
                if p.is_file() {
                    if let Err(e) = indexer::index_file(store, p) {
                        eprintln!("index error for {:?}: {e}", p);
                    }
                }
            }
        }
        EventKind::Remove(_) => {
            for p in &event.paths {
                if let Err(e) = indexer::remove_file(store, p) {
                    eprintln!("remove error for {:?}: {e}", p);
                }
            }
        }
        _ => {}
    }
}
