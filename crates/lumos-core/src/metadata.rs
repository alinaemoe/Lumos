use crate::storage::ProjectStore;
use anyhow::Result;
use std::path::Path;

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

/// Creates the project if it doesn't exist yet, returns its id.
pub fn get_or_create_project(store: &impl ProjectStore, name: &str) -> Result<i64> {
    store.get_or_create_project(name)
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
pub fn register_project_path(
    store: &impl ProjectStore,
    project_id: i64,
    path: &Path,
) -> Result<()> {
    let path_str = normalize_path(path);
    let repo_type = detect_repo_type(path);
    store.register_project_path(project_id, &path_str, repo_type.as_deref())
}

/// Finds which project owns a given file path by matching against all
/// registered project_paths and picking the longest (most specific) prefix.
///
/// The storage layer only supplies the candidate list — the choice of
/// *which* path wins is a domain rule, so it lives here and is testable
/// without a database (see `pick_longest_prefix`).
pub fn resolve_project_for_path(
    store: &impl ProjectStore,
    file_path: &Path,
) -> Result<Option<i64>> {
    let file_path_str = normalize_path(file_path);
    let candidates = store.list_project_paths()?;
    Ok(pick_longest_prefix(&file_path_str, &candidates))
}

/// Longest-prefix match: the most specific registered root wins, so a
/// repo nested inside another registered folder claims its own files.
pub(crate) fn pick_longest_prefix(
    file_path: &str,
    candidates: &[crate::storage::ProjectPathEntry],
) -> Option<i64> {
    let mut best: Option<(i64, usize)> = None;
    for entry in candidates {
        if file_path.starts_with(&entry.path) {
            let len = entry.path.len();
            if best.map(|(_, best_len)| len > best_len).unwrap_or(true) {
                best = Some((entry.project_id, len));
            }
        }
    }
    best.map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ProjectPathEntry;

    fn entry(project_id: i64, path: &str) -> ProjectPathEntry {
        ProjectPathEntry { project_id, path: path.into() }
    }

    #[test]
    fn most_specific_registered_root_wins() {
        let candidates = vec![
            entry(1, "C:\\work"),
            entry(2, "C:\\work\\lumos"),
            entry(3, "C:\\other"),
        ];
        assert_eq!(
            pick_longest_prefix("C:\\work\\lumos\\src\\main.rs", &candidates),
            Some(2)
        );
        assert_eq!(pick_longest_prefix("C:\\work\\notes.md", &candidates), Some(1));
        assert_eq!(pick_longest_prefix("D:\\elsewhere\\x", &candidates), None);
    }
}
