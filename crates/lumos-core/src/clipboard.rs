use crate::search::sanitize_fts_query;
use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// A multi-megabyte clipboard payload (someone copied a whole log file) has
/// no business being hashed into the FTS index in full — cap it, but well
/// above anything a token/JSON/stack-trace copy would ever hit in practice.
const MAX_TEXT_BYTES: usize = 200_000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClipboardResult {
    pub id: i64,
    pub text_content: String,
    pub detected_type: String,
    pub created_at: i64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn jwt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$").unwrap())
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .unwrap()
    })
}

const SQL_KEYWORDS: &[&str] = &[
    "select", "insert into", "update", "delete from", "create table", "alter table",
    "drop table", "create index",
];

/// Best-effort classification by cheap, specific pattern first — a JWT is
/// unambiguous, so it's checked before the much looser "starts with {"
/// JSON heuristic could ever get a chance to misfire on it.
pub fn classify(text: &str) -> &'static str {
    let trimmed = text.trim();

    if jwt_re().is_match(trimmed) {
        return "jwt";
    }
    if uuid_re().is_match(trimmed) {
        return "uuid";
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
        return "json";
    }
    let lower = trimmed.to_lowercase();
    if SQL_KEYWORDS.iter().any(|kw| lower.starts_with(kw)) {
        return "sql";
    }
    if trimmed.contains("Exception")
        || trimmed.contains("Traceback (most recent call last)")
        || trimmed.contains("\n  at ")
        || trimmed.starts_with("at ")
    {
        return "stacktrace";
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return "url";
    }
    "text"
}

/// Records a text clipboard entry unless it's empty or identical to the
/// most recent one — apps that "refresh" the clipboard with the same
/// content (and simple repeated copies) would otherwise flood the history
/// with duplicates of the exact same thing. Returns the new row id, or
/// `None` if nothing was recorded.
pub fn record_text(conn: &Connection, text: &str) -> Result<Option<i64>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let capped = truncate_utf8(trimmed, MAX_TEXT_BYTES);

    let last: Option<String> = conn
        .query_row(
            "SELECT text_content FROM clipboard_entries
             WHERE content_type = 'text'
             ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("checking last clipboard entry")?;
    if last.as_deref() == Some(capped) {
        return Ok(None);
    }

    let detected = classify(capped);
    conn.execute(
        "INSERT INTO clipboard_entries (content_type, text_content, detected_type, created_at)
         VALUES ('text', ?1, ?2, ?3)",
        rusqlite::params![capped, detected, now()],
    )
    .context("inserting clipboard entry")?;

    Ok(Some(conn.last_insert_rowid()))
}

pub fn search(conn: &Connection, query: &str, limit: i64) -> Result<Vec<ClipboardResult>> {
    let fts_query = sanitize_fts_query(query);
    if fts_query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT c.id, c.text_content, c.detected_type, c.created_at
         FROM clipboard_fts
         JOIN clipboard_entries c ON c.id = clipboard_fts.rowid
         WHERE clipboard_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![fts_query, limit], |row| {
        Ok(ClipboardResult {
            id: row.get(0)?,
            text_content: row.get(1)?,
            detected_type: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn recent(conn: &Connection, limit: i64) -> Result<Vec<ClipboardResult>> {
    let mut stmt = conn.prepare(
        "SELECT id, text_content, detected_type, created_at
         FROM clipboard_entries
         WHERE content_type = 'text'
         ORDER BY id DESC
         LIMIT ?1",
    )?;

    let rows = stmt.query_map([limit], |row| {
        Ok(ClipboardResult {
            id: row.get(0)?,
            text_content: row.get(1)?,
            detected_type: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}
