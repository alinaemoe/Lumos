//! The concrete, Ollama-backed [`SchemaInterpreter`] — Phase 5 Step 5.3.
//!
//! Per `LUMOS_DESIGN.md` §5/§6, the AI Interpreter is **Ollama, a separate
//! local HTTP process, not embedded in the Rust core** ("Вариант A" —
//! confirmed against the design doc before this step began, not a new
//! decision). This module is the HTTP client for that process and nothing
//! more: `interpreter::SchemaInterpreter::generate` takes a query, returns
//! raw text, and every rejection becomes an `Err`. It never sees a
//! `Connection`, a filesystem path, or a [`crate::search::SearchIntent`] —
//! there is no parameter here one could arrive through, by construction of
//! [`SchemaInterpreter`]'s own signature (see `interpreter.rs`'s doc
//! comment for why that trait is shaped this narrowly).
//!
//! ```text
//! query + prompt + model
//!         |
//!         v
//! HTTP POST http://<base_url>/api/generate   (Ollama's single-shot generate endpoint)
//!         |
//!         v
//! { "response": "<model's raw text>", ... }
//!         |
//!         v
//! Result<String>   <- everything past this point (JSON validation, schema
//!                      version, source/token checks) is `intent_schema.rs`'s
//!                      job, called from `interpreter::interpret`, never
//!                      from here.
//! ```
//!
//! The prompt below *quotes* the schema's vocabulary (current version,
//! source names, time tokens) by reading it from `intent_schema.rs`'s own
//! constants/enums rather than retyping it as a second, driftable copy —
//! but it is guidance text for the model, not a validator. A model that
//! ignores every rule in it still only ever produces a `String`; whether
//! that string is acceptable is decided exactly once, downstream, by
//! `intent_schema::parse_schema_json`.
//!
//! No dependency on `rusqlite`, `storage`, `storage_sqlite`, or `search`'s
//! `search_all`/`SearchIntent` appears anywhere below — verified by reading
//! this file's own `use` list, which is the entire dependency surface.

use crate::intent_schema::{SourceName, TimeToken, SCHEMA_VERSION};
use crate::interpreter::SchemaInterpreter;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Where to find Ollama and which model to ask. Defaults match
/// `LUMOS_DESIGN.md` §6 exactly: localhost, no external API calls, the
/// smaller of the two named models — a 4B model is the one already sized
/// for "CPU-inference acceptable for on-demand, not streaming background".
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// e.g. `http://localhost:11434`. No trailing slash required — it's
    /// stripped before use.
    pub base_url: String,
    pub model: String,
    /// Finite by contract: the CLI/app must never block indefinitely on a
    /// local process that may not be running. A timeout always surfaces as
    /// `Err`, which `interpreter::interpret` treats identically to every
    /// other failure mode — see that function's fallback doc comment.
    pub timeout: Duration,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".to_string(),
            model: "qwen3:4b".to_string(),
            timeout: Duration::from_secs(15),
        }
    }
}

/// Transport-only [`SchemaInterpreter`]. Holds configuration and nothing
/// else — no cached connection, no state carried between calls, matching
/// `intent_schema.rs`'s own "pure functions, no hidden state" shape.
pub struct OllamaInterpreter {
    config: OllamaConfig,
}

impl OllamaInterpreter {
    pub fn new(config: OllamaConfig) -> Self {
        Self { config }
    }
}

/// The subset of Ollama's `/api/generate` response this client reads.
/// Deliberately minimal — `response` is the only field used; everything
/// else Ollama sends (`model`, `created_at`, `done`, timing stats) is
/// ignored rather than modeled, since none of it is needed to satisfy
/// [`SchemaInterpreter`]'s contract of "return the generated text or an
/// error."
#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    #[serde(default)]
    response: String,
}

#[derive(Debug, Serialize)]
struct OllamaGenerateRequest<'a> {
    model: &'a str,
    prompt: String,
    format: &'a str,
    stream: bool,
}

/// Builds the prompt sent to the model. Reads the schema's own constants
/// (`SCHEMA_VERSION`, [`SourceName`], [`TimeToken`]) rather than hand-typing
/// their values a second time, so this text cannot silently drift out of
/// sync with what `intent_schema::parse_schema_json` will actually accept —
/// though drift would only ever make the model's job harder (asking for a
/// stale version, an unlisted source), never unsafe: the parser downstream
/// is what actually enforces every rule stated here, not this string.
fn build_prompt(query: &str) -> String {
    let sources: Vec<String> = [SourceName::File, SourceName::Clipboard]
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_default().trim_matches('"').to_string())
        .collect();
    let time_tokens: Vec<String> = [
        TimeToken::Today,
        TimeToken::Yesterday,
        TimeToken::LastWeek,
        TimeToken::LastMonth,
    ]
    .iter()
    .map(|t| serde_json::to_string(t).unwrap_or_default().trim_matches('"').to_string())
    .collect();

    format!(
        "You translate a user's natural-language search query into a single JSON object \
         for a local search tool. Output ONLY the JSON object — no prose, no markdown fences, \
         no explanation.\n\
         \n\
         Fields (all optional except version):\n\
         - version: integer, must be exactly {SCHEMA_VERSION}\n\
         - text: string — free-text search terms\n\
         - sources: array of strings, each one of {sources:?} — omit or leave empty to mean \"everywhere\"\n\
         - content_kind: string — a content classification such as \"jwt\", \"sql\", \"url\" (omit if none applies)\n\
         - time: string, one of {time_tokens:?}\n\
         - project: string — a project NAME (never a numeric id)\n\
         \n\
         Rules:\n\
         - Never output a numeric timestamp or epoch of any kind.\n\
         - Never output a database id.\n\
         - Never output SQL or search-operator syntax (no \"type:\", no \"AND\"/\"OR\").\n\
         - Use only the fields listed above; any other field will be rejected.\n\
         \n\
         User query: {query}"
    )
}

impl SchemaInterpreter for OllamaInterpreter {
    /// Sends `query` to Ollama and returns whatever text it generated.
    /// Every failure — connection refused, non-2xx status, timeout,
    /// malformed JSON, an empty `response` field — becomes `Err`. Nothing
    /// here inspects the *content* of that text (no JSON-schema check, no
    /// field check) — that is `intent_schema::parse_schema_json`'s job,
    /// invoked by `interpreter::interpret`, never by this function.
    fn generate(&self, query: &str) -> Result<String> {
        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/api/generate");
        let body = OllamaGenerateRequest {
            model: &self.config.model,
            prompt: build_prompt(query),
            // Ollama's structured-output flag: constrains sampling toward
            // well-formed JSON. Still just a hint to the model — the actual
            // acceptance gate remains `intent_schema::parse_schema_json`.
            format: "json",
            stream: false,
        };

        let response = ureq::post(&url)
            .timeout(self.config.timeout)
            .send_json(&body)
            .map_err(|e| anyhow!("ollama request to {url} failed: {e}"))?;

        let parsed: OllamaGenerateResponse = response
            .into_json()
            .context("ollama response was not the expected JSON shape")?;

        let text = parsed.response.trim();
        if text.is_empty() {
            bail!("ollama returned no generated content");
        }
        Ok(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::interpret;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// A minimal loopback HTTP/1.1 server: accepts one connection, records
    /// the raw request bytes it received, writes back a fixed response,
    /// then closes. Not a general-purpose mock — just enough of HTTP to
    /// exercise `ureq`'s client path deterministically and offline, since
    /// the repository has no existing HTTP-mocking dependency and this step
    /// is instructed not to add one solely for this test.
    struct MockServer {
        base_url: String,
        received: Arc<Mutex<Option<String>>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl MockServer {
        /// `respond_after` lets the timeout test simulate a slow Ollama.
        fn start(response: Vec<u8>, respond_after: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(None));
            let received_clone = received.clone();
            let handle = thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream.set_read_timeout(Some(Duration::from_millis(500))).ok();
                    // Read until the client stops sending (0.5s of silence,
                    // via the read timeout above, or a closed connection) —
                    // not until a partial read, which can fire after only
                    // the headers have arrived if the body lands in a
                    // second TCP segment.
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(_) => break,
                        }
                    }
                    *received_clone.lock().unwrap() = Some(String::from_utf8_lossy(&buf).to_string());
                    thread::sleep(respond_after);
                    let _ = stream.write_all(&response);
                }
            });
            Self { base_url, received, handle: Some(handle) }
        }

        fn join(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }

        fn request_text(&self) -> String {
            self.received.lock().unwrap().clone().unwrap_or_default()
        }
    }

    fn http_ok(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    fn http_status(code: u16, reason: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    fn config_for(base_url: &str) -> OllamaConfig {
        OllamaConfig { base_url: base_url.to_string(), model: "testmodel".to_string(), timeout: Duration::from_secs(5) }
    }

    #[test]
    fn successful_response_returns_generated_text() {
        let mut server = MockServer::start(http_ok(r#"{"response":"{\"version\":1}","done":true}"#), Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        let text = client.generate("find my jwt").unwrap();
        assert_eq!(text, r#"{"version":1}"#);
        server.join();
    }

    #[test]
    fn http_error_status_returns_err() {
        let mut server = MockServer::start(http_status(500, "Internal Server Error", r#"{"error":"boom"}"#), Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn connection_refused_returns_err() {
        // Bind then immediately drop: the port is free again, but nothing
        // is listening on it, which is exactly "connection refused."
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let client = OllamaInterpreter::new(config_for(&base_url));
        assert!(client.generate("q").is_err());
    }

    #[test]
    fn malformed_json_body_returns_err() {
        let mut server = MockServer::start(http_ok("not json at all"), Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn empty_generated_content_returns_err() {
        let mut server = MockServer::start(http_ok(r#"{"response":""}"#), Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn missing_response_field_returns_err() {
        let mut server = MockServer::start(http_ok(r#"{"done":true}"#), Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn slow_server_beyond_timeout_returns_err() {
        // Server deliberately responds well after the client's configured
        // timeout — this is the "Ollama hung / is overloaded" case, not
        // "Ollama is down."
        let mut server = MockServer::start(http_ok(r#"{"response":"{}"}"#), Duration::from_secs(3));
        let mut config = config_for(&server.base_url);
        config.timeout = Duration::from_millis(200);
        let client = OllamaInterpreter::new(config);

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn request_targets_configured_model() {
        let mut server = MockServer::start(http_ok(r#"{"response":"{\"version\":1}"}"#), Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));
        client.generate("q").unwrap();

        server.join();
        assert!(server.request_text().contains(r#""model":"testmodel""#));
    }

    #[test]
    fn request_declares_json_format() {
        let mut server = MockServer::start(http_ok(r#"{"response":"{\"version\":1}"}"#), Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));
        client.generate("q").unwrap();

        server.join();
        assert!(server.request_text().contains(r#""format":"json""#));
    }

    #[test]
    fn request_goes_to_the_configured_base_url() {
        // The mock server's address is a fresh, randomly assigned loopback
        // port each run; the request reaching it at all (rather than some
        // hardcoded default) proves `base_url` is actually respected.
        let mut server = MockServer::start(http_ok(r#"{"response":"{\"version\":1}"}"#), Duration::ZERO);
        let host_header = server.base_url.trim_start_matches("http://").to_string();
        let client = OllamaInterpreter::new(config_for(&server.base_url));
        client.generate("q").unwrap();

        server.join();
        let req = server.request_text();
        assert!(req.contains("POST /api/generate"));
        assert!(req.contains(&host_header), "request must be addressed to the configured base_url's host:port");
    }

    #[test]
    fn ollama_client_failure_reaches_the_deterministic_fallback() {
        // The end-to-end proof required by this step: wiring a failing
        // OllamaInterpreter into `interpreter::interpret` must produce
        // exactly what `parse_intent` alone would — the client's failure
        // must never surface as an application-level error.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let client = OllamaInterpreter::new(config_for(&base_url));
        let no_projects = |_: &str| None;
        let intent = interpret(Some(&client), "type:jwt", crate::now_secs(), &no_projects);

        assert_eq!(intent, crate::search::parse_intent("type:jwt"));
    }
}
