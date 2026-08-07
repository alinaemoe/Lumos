use crate::object::{ObjectId, ObjectType};
use crate::search::sanitize_fts_query;
use crate::storage::{ClipboardInsert, ClipboardStore, EmbeddingStore};
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
    pub source_app: Option<String>,
    pub pinned: bool,
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

macro_rules! static_re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pat).unwrap())
        }
    };
}

static_re!(jwt_re, r"^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$");
static_re!(
    uuid_re,
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
);
// Matches a DB connection URI *anywhere* in the text, not just when it's the
// whole clipboard — "коннекшн стринг для монги: mongodb://..." is a normal
// way to actually copy one of these.
static_re!(
    conn_string_re,
    r"(?i)\b(mongodb(\+srv)?|postgres(ql)?|mysql|redis|amqp|mssql|sqlserver)://\S+"
);
static_re!(
    secret_re,
    r"(?i)\b(AKIA[0-9A-Z]{16}|sk_(live|test)_[0-9a-zA-Z]{10,}|pk_(live|test)_[0-9a-zA-Z]{10,}|gh[pousr]_[0-9a-zA-Z]{20,}|xox[baprs]-[0-9a-zA-Z-]{10,}|Bearer\s+[A-Za-z0-9_\-.]{15,})"
);
static_re!(hash_re, r"^[0-9a-fA-F]{32}$|^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$");
static_re!(
    ip_re,
    r"^(\d{1,3}\.){3}\d{1,3}$|^([0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}$"
);
static_re!(
    email_re,
    r"^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$"
);
static_re!(
    file_path_re,
    r"^([A-Za-z]:\\[^\r\n]+|~?/[^\r\n]+|\\\\[^\r\n]+)$"
);
static_re!(
    color_re,
    r"^#[0-9a-fA-F]{3}([0-9a-fA-F]{3})?$|^rgba?\(\s*\d+\s*,\s*\d+\s*,\s*\d+"
);
static_re!(
    iso_date_re,
    r"^\d{4}-\d{2}-\d{2}([T ]\d{2}:\d{2}(:\d{2})?(\.\d+)?(Z|[+-]\d{2}:?\d{2})?)?$"
);
static_re!(env_line_re, r"^[A-Z][A-Z0-9_]*=\S+$");

/// First word of common CLI tools — checked after stripping a shell/prompt
/// prefix, so `$ git status`, `PS C:\> npm run build` and plain `docker ps`
/// all match. Deliberately broad ("maximum coverage" was the ask): version
/// control, package managers, containers/orchestration, cloud CLIs, build
/// tools, runtimes, and common *nix/PowerShell system commands.
const TERMINAL_COMMANDS: &[&str] = &[
    "git", "npm", "npx", "yarn", "pnpm", "pip", "pip3", "cargo", "gem", "composer", "nuget",
    "choco", "winget", "brew", "apt", "apt-get", "yum", "dnf", "docker", "docker-compose",
    "kubectl", "helm", "k9s", "minikube", "aws", "az", "gcloud", "terraform", "ansible",
    "vagrant", "make", "cmake", "mvn", "gradle", "ant", "msbuild", "node", "python", "python3",
    "ruby", "php", "dotnet", "java", "go", "deno", "bun", "ssh", "scp", "rsync", "curl", "wget",
    "ping", "traceroute", "tracert", "nslookup", "netstat", "ipconfig", "ifconfig", "ps", "kill",
    "chmod", "chown", "sudo", "systemctl", "service", "journalctl", "tail", "grep", "sed", "awk",
    "find", "ls", "dir", "cd", "mkdir", "rmdir", "rm", "del", "cp", "mv", "cat", "code", "vim",
    "nano", "psql", "mysql", "mongosh", "redis-cli",
];

fn strip_shell_prompt(line: &str) -> &str {
    let line = line.trim_start();
    for prefix in ["$ ", "> ", "# "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest;
        }
    }
    // PowerShell prompt: "PS C:\some\path>" optionally followed by a space.
    if let Some(idx) = line.find("> ") {
        if line[..idx].starts_with("PS ") {
            return &line[idx + 2..];
        }
    }
    line
}

fn is_terminal_command(trimmed: &str) -> bool {
    let first_line = trimmed.lines().next().unwrap_or("");
    let stripped = strip_shell_prompt(first_line);
    let first_word = stripped.split_whitespace().next().unwrap_or("");
    TERMINAL_COMMANDS.contains(&first_word)
        || (first_word.starts_with("Get-")
            || first_word.starts_with("Set-")
            || first_word.starts_with("New-")
            || first_word.starts_with("Remove-")
            || first_word.starts_with("Invoke-")
            || first_word.starts_with("Start-")
            || first_word.starts_with("Stop-"))
}

fn looks_like_env_block(trimmed: &str) -> bool {
    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
    !lines.is_empty() && lines.iter().all(|l| env_line_re().is_match(l.trim()))
}

fn looks_like_yaml(trimmed: &str) -> bool {
    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.len() >= 2
        && !trimmed.starts_with('{')
        && !trimmed.starts_with('<')
        && lines
            .iter()
            .filter(|l| {
                let t = l.trim_start();
                t.contains(':') && !t.starts_with("http://") && !t.starts_with("https://")
            })
            .count()
            >= lines.len() / 2
}

const SQL_KEYWORDS: &[&str] = &[
    "select", "insert into", "update", "delete from", "create table", "alter table",
    "drop table", "create index",
];

/// Type-name search ("find everything tagged jwt/sql/error") needs the
/// type — and a couple of the words someone would actually type instead of
/// the internal label — to be indexed, not just the raw copied text.
fn type_synonyms(detected_type: &str) -> &'static str {
    match detected_type {
        "jwt" => "jwt token auth",
        "uuid" => "uuid guid id identifier",
        "sql" => "sql query database",
        "json" => "json object data",
        "stacktrace" => "error exception stacktrace crash",
        "url" => "url link",
        "connection_string" => "connection string database uri dsn",
        "secret" => "secret credential key token api",
        "hash" => "hash checksum digest",
        "ip_address" => "ip address network",
        "email" => "email address contact mail",
        "file_path" => "file path filesystem location",
        "terminal_command" => "command terminal shell cli console",
        "env_block" => "env environment config variables",
        "yaml" => "yaml config",
        "xml_html" => "xml html markup",
        "color" => "color colour hex rgb",
        "date" => "date time timestamp",
        _ => "",
    }
}

/// A JWT's payload is base64url — readable claims like a username are
/// invisible to plain-text search until decoded. No signature verification
/// here; this is purely "make it findable later", not an auth boundary.
fn decode_jwt_payload(text: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let parts: Vec<&str> = text.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    String::from_utf8(decoded).ok()
}

/// What actually gets indexed alongside the raw text — the detected type,
/// its search synonyms, and (for JWTs) the decoded payload so claims inside
/// the token are findable without showing the decoded form in the UI.
fn build_search_extra(detected_type: &str, text: &str) -> String {
    let mut extra = type_synonyms(detected_type).to_string();
    if detected_type == "jwt" {
        if let Some(payload) = decode_jwt_payload(text) {
            extra.push(' ');
            extra.push_str(&payload);
        }
    }
    extra
}

/// All known type labels, in classification precedence order (most
/// specific/unambiguous first). Exposed so the CLI/UI can list valid
/// `type:` filter values without duplicating this list by hand.
pub const KNOWN_TYPES: &[&str] = &[
    "jwt", "uuid", "secret", "hash", "ip_address", "email", "json", "sql", "stacktrace",
    "connection_string", "terminal_command", "env_block", "yaml", "xml_html", "file_path", "url",
    "color", "date", "text",
];

/// Best-effort classification by cheap, specific pattern first — exact,
/// unambiguous formats (JWT, UUID, hash, secret prefixes) are checked
/// before looser heuristics (YAML, env-block) that could misfire on
/// almost-anything multi-line text.
pub fn classify(text: &str) -> &'static str {
    let trimmed = text.trim();

    if jwt_re().is_match(trimmed) {
        return "jwt";
    }
    if uuid_re().is_match(trimmed) {
        return "uuid";
    }
    if secret_re().is_match(trimmed) {
        return "secret";
    }
    if hash_re().is_match(trimmed) {
        return "hash";
    }
    if ip_re().is_match(trimmed) {
        return "ip_address";
    }
    if email_re().is_match(trimmed) {
        return "email";
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
    if conn_string_re().is_match(trimmed) {
        return "connection_string";
    }
    if is_terminal_command(trimmed) {
        return "terminal_command";
    }
    if looks_like_env_block(trimmed) {
        return "env_block";
    }
    if looks_like_yaml(trimmed) {
        return "yaml";
    }
    if trimmed.starts_with('<') {
        return "xml_html";
    }
    if file_path_re().is_match(trimmed) {
        return "file_path";
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return "url";
    }
    if color_re().is_match(trimmed) {
        return "color";
    }
    if iso_date_re().is_match(trimmed) {
        return "date";
    }
    "text"
}

/// Records a text clipboard entry unless it's empty or identical to the
/// most recent one — apps that "refresh" the clipboard with the same
/// content (and simple repeated copies) would otherwise flood the history
/// with duplicates of the exact same thing. `source_app` is whatever the
/// caller could determine was in the foreground at copy time (best-effort,
/// `None` if unavailable) — this crate stays OS-agnostic and never queries
/// the foreground window itself. Returns the new row id, or `None` if
/// nothing was recorded.
pub fn record_text<S>(store: &S, text: &str, source_app: Option<&str>) -> Result<Option<i64>>
where
    S: ClipboardStore + EmbeddingStore,
{
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let capped = truncate_utf8(trimmed, MAX_TEXT_BYTES);

    // The duplicate *decision* is a domain rule; storage only supplies the
    // text to compare against.
    if store.latest_text()?.as_deref() == Some(capped) {
        return Ok(None);
    }

    let detected = classify(capped);
    let id = store.insert_entry(&ClipboardInsert {
        text: capped.to_string(),
        detected_type: detected.to_string(),
        search_extra: build_search_extra(detected, capped),
        source_app: source_app.map(str::to_string),
        created_at: now(),
    })?;

    // Best-effort, same reasoning as the file indexer: a missed/failed
    // embedding degrades semantic search quietly, not clipboard recording.
    let _ = crate::embeddings::store(store, ObjectId::new(ObjectType::Clipboard, id), capped);

    Ok(Some(id))
}

fn row_to_result(row: &rusqlite::Row) -> rusqlite::Result<ClipboardResult> {
    Ok(ClipboardResult {
        id: row.get(0)?,
        text_content: row.get(1)?,
        detected_type: row.get(2)?,
        source_app: row.get(3)?,
        pinned: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
    })
}

pub fn by_id(conn: &Connection, id: i64) -> Result<Option<ClipboardResult>> {
    conn.query_row(
        "SELECT id, text_content, detected_type, source_app, pinned, created_at
         FROM clipboard_entries WHERE id = ?1",
        [id],
        row_to_result,
    )
    .optional()
    .map_err(Into::into)
}

/// Optional filters extracted from a `type:xxx` / `today` / `yesterday` /
/// `week` query prefix — see `search::parse_filters`. Kept here (rather
/// than making callers build SQL themselves) so the WHERE-clause wiring
/// lives next to the schema it targets.
#[derive(Debug, Default, Clone)]
pub struct Filters {
    pub detected_type: Option<String>,
    pub since: Option<i64>,
}

pub fn search(conn: &Connection, query: &str, filters: &Filters, limit: i64) -> Result<Vec<ClipboardResult>> {
    let fts_query = sanitize_fts_query(query);
    let has_text = !fts_query.trim().is_empty();
    if !has_text && filters.detected_type.is_none() && filters.since.is_none() {
        return Ok(Vec::new());
    }

    let mut sql = String::from(
        "SELECT c.id, c.text_content, c.detected_type, c.source_app, c.pinned, c.created_at FROM clipboard_entries c",
    );
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if has_text {
        sql.push_str(" JOIN clipboard_fts ON clipboard_fts.rowid = c.id");
        conditions.push("clipboard_fts MATCH ?".to_string());
        params.push(Box::new(fts_query));
    }
    if let Some(t) = &filters.detected_type {
        conditions.push("c.detected_type = ?".to_string());
        params.push(Box::new(t.clone()));
    }
    if let Some(since) = filters.since {
        conditions.push("c.created_at >= ?".to_string());
        params.push(Box::new(since));
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(if has_text {
        " ORDER BY rank LIMIT ?"
    } else {
        " ORDER BY c.id DESC LIMIT ?"
    });
    params.push(Box::new(limit));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), row_to_result)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn recent(conn: &Connection, limit: i64) -> Result<Vec<ClipboardResult>> {
    let mut stmt = conn.prepare(
        "SELECT id, text_content, detected_type, source_app, pinned, created_at
         FROM clipboard_entries
         WHERE content_type = 'text'
         ORDER BY id DESC
         LIMIT ?1",
    )?;

    let rows = stmt.query_map([limit], row_to_result)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn set_pinned(conn: &Connection, id: i64, pinned: bool) -> Result<()> {
    conn.execute(
        "UPDATE clipboard_entries SET pinned = ?1 WHERE id = ?2",
        rusqlite::params![pinned as i64, id],
    )
    .context("updating pinned flag")?;
    Ok(())
}

/// Deletes text clipboard entries older than `days`, skipping anything
/// pinned — retention is a privacy/bloat control, not a reason to lose
/// something the user deliberately kept. Returns the number of rows removed.
pub fn purge_older_than(conn: &Connection, days: i64) -> Result<usize> {
    let cutoff = now() - days * 86_400;
    let removed = conn
        .execute(
            "DELETE FROM clipboard_entries WHERE created_at < ?1 AND pinned = 0",
            [cutoff],
        )
        .context("purging old clipboard entries")?;
    // No FK between embeddings and clipboard_entries (it's a generic
    // object_type/object_id table shared with files), so orphans need an
    // explicit sweep rather than an ON DELETE CASCADE.
    conn.execute(
        "DELETE FROM embeddings WHERE object_type = 'clipboard' AND object_id NOT IN (SELECT id FROM clipboard_entries)",
        [],
    )
    .context("cleaning orphaned clipboard embeddings")?;
    Ok(removed)
}
