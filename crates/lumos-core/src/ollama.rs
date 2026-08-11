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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Transport-only [`SchemaInterpreter`], plus one bit of process-lifetime
/// state: whether a *transport-level* failure (connection refused, DNS
/// failure, timeout establishing/completing the connection) has already
/// been observed. See [`generate`](SchemaInterpreter::generate)'s doc
/// comment below for why only that failure class sets it, and
/// `ARCHITECTURE_INVARIANTS.md`'s Phase 5.5 entry for the reasoning this
/// design is based on.
///
/// One `OllamaInterpreter` is constructed once per process (CLI: in
/// `main()`; app: once in `setup()`, held in Tauri-managed state), so
/// "process-lifetime" and "this struct's lifetime" are the same span in
/// practice — the flag needs no explicit reset path, because a new process
/// means a new instance means `unavailable` starts at `false` again.
pub struct OllamaInterpreter {
    config: OllamaConfig,
    unavailable: AtomicBool,
}

impl OllamaInterpreter {
    pub fn new(config: OllamaConfig) -> Self {
        Self { config, unavailable: AtomicBool::new(false) }
    }
}

/// A *transport*-level failure — no HTTP response was ever received
/// (connection refused, DNS failure, TLS failure, the connect/read/write
/// timing out). Distinct from [`ureq::Error::Status`], which means Ollama
/// **did** answer, just with a non-2xx status — that is "available, but
/// something else is wrong," not "unavailable," and must not trip the same
/// flag (Phase 5.5 requirement D: a live-but-misbehaving server is not the
/// same fact as an absent one).
fn is_transport_failure(error: &ureq::Error) -> bool {
    matches!(error, ureq::Error::Transport(_))
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
    ///
    /// Phase 5.5: skips the network call entirely once a transport-level
    /// failure has already been observed this process — see the struct's
    /// own doc comment. This is the one and only place that check happens;
    /// every other failure mode (bad status, malformed body, empty
    /// content) still runs the request fresh every time, because those say
    /// nothing about whether Ollama is *reachable*, only about what it said
    /// once reached.
    fn generate(&self, query: &str) -> Result<String> {
        if self.unavailable.load(Ordering::Relaxed) {
            bail!("ollama was found unreachable earlier this session — skipping connection attempt");
        }

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

        let response = match ureq::post(&url).timeout(self.config.timeout).send_json(&body) {
            Ok(response) => response,
            Err(e) => {
                if is_transport_failure(&e) {
                    self.unavailable.store(true, Ordering::Relaxed);
                }
                return Err(anyhow!("ollama request to {url} failed: {e}"));
            }
        };

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

    /// A minimal loopback HTTP/1.1 server: accepts up to `responses.len()`
    /// connections in order, records the raw request bytes each one sent,
    /// writes back the corresponding canned response, then moves to the
    /// next. Not a general-purpose mock — just enough of HTTP to exercise
    /// `ureq`'s client path deterministically and offline, since the
    /// repository has no existing HTTP-mocking dependency and this step is
    /// instructed not to add one solely for this test.
    ///
    /// `accepted_count()` is what Phase 5.5's tests assert on instead of
    /// timing: how many real connections actually reached the server,
    /// which is the direct, non-flaky observation of "did the client skip
    /// the network or not."
    struct MockServer {
        base_url: String,
        received: Arc<Mutex<Vec<String>>>,
        accepted: Arc<std::sync::atomic::AtomicUsize>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl MockServer {
        /// `respond_after` lets the timeout test simulate a slow Ollama —
        /// applied uniformly before every response, since no current test
        /// needs a different delay per response.
        fn start(responses: Vec<Vec<u8>>, respond_after: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(Vec::new()));
            let received_clone = received.clone();
            let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let accepted_clone = accepted.clone();
            let handle = thread::spawn(move || {
                for response in responses {
                    let Ok((mut stream, _)) = listener.accept() else { break };
                    accepted_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                    received_clone.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
                    thread::sleep(respond_after);
                    let _ = stream.write_all(&response);
                }
            });
            Self { base_url, received, accepted, handle: Some(handle) }
        }

        fn join(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }

        /// The most recently received request — the one existing
        /// single-response tests care about.
        fn request_text(&self) -> String {
            self.received.lock().unwrap().last().cloned().unwrap_or_default()
        }

        fn accepted_count(&self) -> usize {
            self.accepted.load(std::sync::atomic::Ordering::SeqCst)
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
        let mut server = MockServer::start(vec![http_ok(r#"{"response":"{\"version\":1}","done":true}"#)], Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        let text = client.generate("find my jwt").unwrap();
        assert_eq!(text, r#"{"version":1}"#);
        server.join();
    }

    #[test]
    fn http_error_status_returns_err() {
        let mut server = MockServer::start(vec![http_status(500, "Internal Server Error", r#"{"error":"boom"}"#)], Duration::ZERO);
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
        let mut server = MockServer::start(vec![http_ok("not json at all")], Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn empty_generated_content_returns_err() {
        let mut server = MockServer::start(vec![http_ok(r#"{"response":""}"#)], Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn missing_response_field_returns_err() {
        let mut server = MockServer::start(vec![http_ok(r#"{"done":true}"#)], Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn slow_server_beyond_timeout_returns_err() {
        // Server deliberately responds well after the client's configured
        // timeout — this is the "Ollama hung / is overloaded" case, not
        // "Ollama is down."
        let mut server = MockServer::start(vec![http_ok(r#"{"response":"{}"}"#)], Duration::from_secs(3));
        let mut config = config_for(&server.base_url);
        config.timeout = Duration::from_millis(200);
        let client = OllamaInterpreter::new(config);

        assert!(client.generate("q").is_err());
        server.join();
    }

    #[test]
    fn request_targets_configured_model() {
        let mut server = MockServer::start(vec![http_ok(r#"{"response":"{\"version\":1}"}"#)], Duration::ZERO);
        let client = OllamaInterpreter::new(config_for(&server.base_url));
        client.generate("q").unwrap();

        server.join();
        assert!(server.request_text().contains(r#""model":"testmodel""#));
    }

    #[test]
    fn request_declares_json_format() {
        let mut server = MockServer::start(vec![http_ok(r#"{"response":"{\"version\":1}"}"#)], Duration::ZERO);
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
        let mut server = MockServer::start(vec![http_ok(r#"{"response":"{\"version\":1}"}"#)], Duration::ZERO);
        let host_header = server.base_url.trim_start_matches("http://").to_string();
        let client = OllamaInterpreter::new(config_for(&server.base_url));
        client.generate("q").unwrap();

        server.join();
        let req = server.request_text();
        assert!(req.contains("POST /api/generate"));
        assert!(req.contains(&host_header), "request must be addressed to the configured base_url's host:port");
    }

    // ---- Phase 5.5: process-lifetime availability state ----

    /// Accepts up to `max_connections`, incrementing `accepted` on each one
    /// and immediately dropping the stream without reading or writing
    /// anything — an abrupt close, which `ureq` observes as a transport
    /// failure client-side, the same failure class as connection-refused.
    /// Lets a test *count* transport-level attempts, which plain
    /// bind-then-drop (used by `connection_refused_returns_err`) cannot,
    /// since nothing is listening there to count with.
    fn abrupt_close_server(max_connections: usize) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_clone = accepted.clone();
        thread::spawn(move || {
            for _ in 0..max_connections {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(_) => break,
                }
            }
        });
        (base_url, accepted)
    }

    #[test]
    fn first_connection_failure_falls_back() {
        let (base_url, _accepted) = abrupt_close_server(1);
        let client = OllamaInterpreter::new(config_for(&base_url));
        assert!(client.generate("q").is_err());
    }

    #[test]
    fn second_query_after_connection_failure_makes_no_further_http_attempt() {
        let (base_url, accepted) = abrupt_close_server(3); // generous cap; a correct client only ever uses 1
        let client = OllamaInterpreter::new(config_for(&base_url));

        assert!(client.generate("first query").is_err());
        // Give the server thread a moment to register the one connection
        // it should see before asserting on the count.
        thread::sleep(Duration::from_millis(150));
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1, "first call must attempt exactly one connection");

        assert!(client.generate("second query").is_err());
        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second call must skip the network entirely once marked unavailable — no second connection attempt"
        );
    }

    #[test]
    fn a_later_success_after_marking_unavailable_is_still_never_attempted() {
        // Documents the accepted limitation, not a bug: once this process
        // has seen a transport failure, it never re-checks Ollama again,
        // even if Ollama comes back up mid-process. Explicitly no
        // background polling/retry per this step's constraints — the flag
        // is one-way for the lifetime of the `OllamaInterpreter` instance.
        let (base_url, accepted) = abrupt_close_server(1);
        let client = OllamaInterpreter::new(config_for(&base_url));

        assert!(client.generate("q1").is_err());
        thread::sleep(Duration::from_millis(150));
        // A real Ollama could now be "up" at this address for all this
        // client knows — it must not care, by design.
        assert!(client.generate("q2").is_err());
        thread::sleep(Duration::from_millis(150));
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_ollama_remains_usable_across_multiple_queries() {
        let mut server = MockServer::start(
            vec![
                http_ok(r#"{"response":"{\"version\":1,\"text\":\"one\"}"}"#),
                http_ok(r#"{"response":"{\"version\":1,\"text\":\"two\"}"}"#),
            ],
            Duration::ZERO,
        );
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert_eq!(client.generate("first").unwrap(), r#"{"version":1,"text":"one"}"#);
        assert_eq!(client.generate("second").unwrap(), r#"{"version":1,"text":"two"}"#);
        server.join();
        assert_eq!(server.accepted_count(), 2, "a healthy Ollama must be asked again every query, never short-circuited");
    }

    #[test]
    fn malformed_json_response_does_not_mark_ollama_unavailable() {
        // Requirement D, case 2: the server answered — it just answered
        // with garbage. That is not the same fact as "unreachable," and
        // must not skip the next attempt.
        let mut server = MockServer::start(
            vec![http_ok("not json at all"), http_ok(r#"{"response":"{\"version\":1}"}"#)],
            Duration::ZERO,
        );
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("first").is_err(), "malformed body must still surface as an error");
        let second = client.generate("second").unwrap();
        assert_eq!(second, r#"{"version":1}"#, "a later well-formed response must still be attempted and used");
        server.join();
        assert_eq!(server.accepted_count(), 2, "malformed content must not have set the unavailable flag");
    }

    #[test]
    fn http_error_status_does_not_mark_ollama_unavailable() {
        // Requirement D, case 2 again, via a non-2xx status rather than a
        // malformed body: the server is alive and answering HTTP, which is
        // a fundamentally different fact from "nothing is listening."
        let mut server = MockServer::start(
            vec![
                http_status(500, "Internal Server Error", r#"{"error":"boom"}"#),
                http_ok(r#"{"response":"{\"version\":1}"}"#),
            ],
            Duration::ZERO,
        );
        let client = OllamaInterpreter::new(config_for(&server.base_url));

        assert!(client.generate("first").is_err());
        let second = client.generate("second").unwrap();
        assert_eq!(second, r#"{"version":1}"#);
        server.join();
        assert_eq!(server.accepted_count(), 2, "a bad HTTP status must not have set the unavailable flag either");
    }

    #[test]
    fn explicit_none_interpreter_makes_zero_http_attempts() {
        // Models what `--no-llm` / `LUMOS_LLM=off` produce at the CLI/app
        // boundary: `schema_interpreter: None`. A live server exists here
        // specifically so the test can prove it is never touched, rather
        // than trusting "well, `None` obviously can't call `generate`" by
        // inspection alone.
        let mut server = MockServer::start(vec![http_ok(r#"{"response":"{\"version\":1}"}"#)], Duration::ZERO);
        let no_projects = |_: &str| None;

        let intent = interpret(None, "type:jwt", crate::now_secs(), &no_projects);

        thread::sleep(Duration::from_millis(150));
        assert_eq!(server.accepted_count(), 0, "an explicitly disabled interpreter must never make an HTTP request");
        assert_eq!(intent, crate::search::parse_intent("type:jwt"));

        // Unblock the server thread's pending accept() so it exits
        // cleanly instead of leaking a blocked thread for the rest of the
        // test binary's run.
        let _ = std::net::TcpStream::connect(server.base_url.trim_start_matches("http://"));
        server.join();
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
