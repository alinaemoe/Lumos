use crate::clipboard::{self, ClipboardResult, Filters};
use anyhow::Result;
use rusqlite::Connection;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchHit {
    File(SearchResult),
    Clipboard(ClipboardResult),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size: i64,
    pub project_name: Option<String>,
}

/// Raw user input can't go straight into an FTS5 MATCH expression — FTS5
/// treats `. - " * : ( )` etc. as query syntax, so a literal search for
/// "CheckoutItem.cs" throws a syntax error instead of just searching for it.
/// Wrapping each word as an escaped quoted phrase forces FTS5 to treat it as
/// literal text; juxtaposed quoted phrases default to AND between them.
pub(crate) fn sanitize_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Pulls `type:xxx` / `today` / `yesterday` / `week` tokens out of the raw
/// query string, leaving whatever's left as the free-text portion. These
/// are relative-hours windows (24h / 48h / 7d back from now), not calendar-
/// day boundaries — exact "midnight to midnight" would need timezone
/// handling we don't have; "at least this recent" is close enough and a lot
/// simpler to reason about (each filter is a strict superset of the
/// previous one, so there's no surprising off-by-one at day boundaries).
pub fn parse_filters(raw: &str) -> (String, Filters) {
    let mut filters = Filters::default();
    let mut remaining = Vec::new();

    for token in raw.split_whitespace() {
        let lower = token.to_lowercase();
        if let Some(rest) = lower.strip_prefix("type:") {
            filters.detected_type = Some(rest.to_string());
        } else if lower == "today" {
            filters.since = Some(now_ts() - 86_400);
        } else if lower == "yesterday" {
            filters.since = Some(now_ts() - 2 * 86_400);
        } else if lower == "week" {
            filters.since = Some(now_ts() - 7 * 86_400);
        } else {
            remaining.push(token);
        }
    }

    (remaining.join(" "), filters)
}

/// Hybrid-ranked-search-to-be: for now this is FTS5 only (filename/path),
/// the v0.2 embedding pass joins in here later without changing callers'
/// shape — both the CLI and the overlay UI go through this one function.
/// `since` (unix seconds) restricts to files modified at or after that
/// time — used by the `today`/`yesterday`/`week` query filters.
pub fn search(conn: &Connection, query: &str, since: Option<i64>, limit: i64) -> Result<Vec<SearchResult>> {
    let fts_query = sanitize_fts_query(query);
    let has_text = !fts_query.trim().is_empty();
    if !has_text && since.is_none() {
        return Ok(Vec::new());
    }

    let mut sql = String::from(
        "SELECT f.path, f.filename, f.extension, f.size, p.name
         FROM files f
         LEFT JOIN projects p ON p.id = f.project_id",
    );
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if has_text {
        sql.push_str(" JOIN files_fts ON files_fts.rowid = f.id");
        conditions.push("files_fts MATCH ?".to_string());
        params.push(Box::new(fts_query));
    }
    if let Some(since) = since {
        conditions.push("f.modified_at >= ?".to_string());
        params.push(Box::new(since));
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(if has_text {
        " ORDER BY rank LIMIT ?"
    } else {
        " ORDER BY f.modified_at DESC LIMIT ?"
    });
    params.push(Box::new(limit));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(SearchResult {
            path: row.get(0)?,
            filename: row.get(1)?,
            extension: row.get(2)?,
            size: row.get(3)?,
            project_name: row.get(4)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Files and clipboard history live in one search box in the UI, so this is
/// the one entry point the overlay actually calls — files first (the more
/// mature, higher-signal source), then clipboard entries filling out the
/// rest of the budget. `type:xxx` is clipboard-only (files don't have a
/// detected_type) and skips the files search entirely — searching "type:jwt"
/// for filenames would just be confusing.
pub fn search_all(conn: &Connection, raw_query: &str, limit: i64) -> Result<Vec<SearchHit>> {
    let (text, filters) = parse_filters(raw_query);

    let files = if filters.detected_type.is_none() {
        search(conn, &text, filters.since, limit)?
    } else {
        Vec::new()
    };
    let remaining = (limit - files.len() as i64).max(0);
    let clips = if remaining > 0 {
        clipboard::search(conn, &text, &filters, remaining)?
    } else {
        Vec::new()
    };

    Ok(files
        .into_iter()
        .map(SearchHit::File)
        .chain(clips.into_iter().map(SearchHit::Clipboard))
        .collect())
}
