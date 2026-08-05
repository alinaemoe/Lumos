use anyhow::Result;
use rusqlite::Connection;

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
fn sanitize_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Hybrid-ranked-search-to-be: for now this is FTS5 only (filename/path),
/// the v0.2 embedding pass joins in here later without changing callers'
/// shape — both the CLI and the overlay UI go through this one function.
pub fn search(conn: &Connection, query: &str, limit: i64) -> Result<Vec<SearchResult>> {
    let fts_query = sanitize_fts_query(query);
    if fts_query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT f.path, f.filename, f.extension, f.size, p.name
         FROM files_fts
         JOIN files f ON f.id = files_fts.rowid
         LEFT JOIN projects p ON p.id = f.project_id
         WHERE files_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![fts_query, limit], |row| {
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
