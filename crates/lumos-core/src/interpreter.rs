//! The interpreter boundary — Phase 5 Step 5.2.
//!
//! This is the seam `search_all` was built for: something turns a raw query
//! into a validated [`SearchIntent`], and `search_all` never learns which
//! something it was. This module is the "something" side of that seam: one
//! trait an eventual LLM client implements, one function that drives it with
//! a deterministic fallback, and — until Step 5.3 wires up Ollama — nothing
//! else. `search::parse_intent` remains the only concrete producer in the
//! app; this module just gives it a rival with an escape hatch.
//!
//! # Why the trait only takes and returns text
//!
//! ```text
//! user query
//!     ↓
//! Interpreter          (this module — text in, text out)
//!     ↓
//! intent_schema         (Step 5.1 — text in, validated SearchIntent out)
//!     ↓
//! validated SearchIntent
//!     ↓
//! search_all(&intent)
//! ```
//!
//! [`SchemaInterpreter::generate`] takes `&str` and returns `Result<String>`
//! — nothing else. It cannot be handed a `Connection`, a `SqliteStore`, or
//! any storage trait, because its signature has no parameter for one; an
//! Ollama HTTP client in Step 5.3 satisfies this trait with an HTTP call and
//! nothing more capable. The raw text it returns is never trusted directly —
//! [`interpret`] always routes it through
//! [`intent_schema::parse_schema_json`], the one path into a
//! [`SearchIntent`] that enforces §5.1/§5.2 (no raw epochs, no raw ids, no
//! unknown fields, no unknown sources/tokens). There is no second, shorter
//! path from model text to `SearchIntent` for a future caller to find and
//! use by mistake.
//!
//! # Fallback
//!
//! [`interpret`] never fails. Every way the model side of this can go
//! wrong — no interpreter configured, `generate` erroring (network down,
//! timeout, whatever Step 5.3's client raises), the returned text not being
//! JSON, the JSON not matching the schema, an unknown source or time token —
//! collapses to the same outcome: fall back to [`search::parse_intent`] on
//! the original raw query. A model failure degrades to today's working
//! search, never to an empty result set and never to a partially-trusted
//! guess built from an invalid response.

use crate::intent_schema;
use crate::search::{self, SearchIntent};
use anyhow::Result;

/// Produces raw model output for a query — the entire LLM-facing surface.
/// Implemented by Step 5.3's Ollama client; for now, only test doubles in
/// this module's own tests implement it.
///
/// `generate` returns text, not a [`SearchIntent`] — turning that text into
/// one, and rejecting it if it doesn't validate, is [`interpret`]'s job, not
/// this trait's implementor's. That split is what makes "the LLM boundary
/// cannot bypass `intent_schema.rs`" true structurally rather than by
/// convention: an implementor has no way to hand back a `SearchIntent`
/// directly even if it wanted to.
pub trait SchemaInterpreter {
    fn generate(&self, query: &str) -> Result<String>;
}

/// Drives an optional [`SchemaInterpreter`] with a deterministic fallback to
/// [`search::parse_intent`].
///
/// `now` and `resolve_project` are threaded straight through to
/// [`intent_schema::IntentSchema::into_intent`] — this function does no
/// clock reads or project lookups of its own, matching that function's own
/// determinism guarantee.
///
/// No `schema_interpreter` (`None`) is the "no LLM configured" case — same
/// code path as any other fallback trigger, not a special case, so today's
/// CLI/app behavior (no interpreter wired in yet) is exactly what
/// `search::parse_intent` already produces.
pub fn interpret(
    schema_interpreter: Option<&dyn SchemaInterpreter>,
    query: &str,
    now: i64,
    resolve_project: &dyn Fn(&str) -> Option<i64>,
) -> SearchIntent {
    if let Some(interpreter) = schema_interpreter {
        if let Ok(raw) = interpreter.generate(query) {
            if let Ok(schema) = intent_schema::parse_schema_json(&raw) {
                return schema.into_intent(now, resolve_project);
            }
        }
    }
    search::parse_intent(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectType;

    fn no_projects(_: &str) -> Option<i64> {
        None
    }

    struct Fixed(&'static str);
    impl SchemaInterpreter for Fixed {
        fn generate(&self, _query: &str) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    struct Failing;
    impl SchemaInterpreter for Failing {
        fn generate(&self, _query: &str) -> Result<String> {
            anyhow::bail!("connection refused")
        }
    }

    #[test]
    fn no_interpreter_configured_uses_deterministic_parser() {
        let intent = interpret(None, "type:jwt", 1_000_000, &no_projects);
        let direct = search::parse_intent("type:jwt");
        assert_eq!(intent.object_types, direct.object_types);
        assert_eq!(intent.detected_type, direct.detected_type);
        assert_eq!(intent.text, direct.text);
    }

    #[test]
    fn successful_interpreter_output_reaches_search_intent() {
        let llm = Fixed(r#"{"version":1,"text":"auth","sources":["clipboard"],"content_kind":"jwt"}"#);
        let intent = interpret(Some(&llm), "find my jwt", 1_000_000, &no_projects);
        assert_eq!(intent.text, "auth");
        assert_eq!(intent.object_types, vec![ObjectType::Clipboard]);
        assert_eq!(intent.detected_type, Some("jwt".into()));
    }

    #[test]
    fn malformed_json_from_interpreter_falls_back_to_deterministic_parser() {
        let llm = Fixed("this is not json");
        let intent = interpret(Some(&llm), "type:jwt", 1_000_000, &no_projects);
        assert_eq!(intent, search::parse_intent("type:jwt"));
    }

    #[test]
    fn invalid_schema_content_falls_back_rather_than_reaching_search_all() {
        // Well-formed JSON, but an unknown source — must never become a
        // SearchIntent with an unsupported object_type in it (that would
        // make retrieve_candidates error out instead of degrading).
        let llm = Fixed(r#"{"version":1,"sources":["screenshot"]}"#);
        let intent = interpret(Some(&llm), "screenshots from yesterday", 1_000_000, &no_projects);
        assert_eq!(intent, search::parse_intent("screenshots from yesterday"));
    }

    #[test]
    fn unknown_time_token_falls_back() {
        let llm = Fixed(r#"{"version":1,"time":"last_tuesday"}"#);
        let intent = interpret(Some(&llm), "last_tuesday stuff", 1_000_000, &no_projects);
        assert_eq!(intent, search::parse_intent("last_tuesday stuff"));
    }

    #[test]
    fn interpreter_failure_falls_back() {
        // Stands in for network-down/timeout — `generate` erroring is the
        // one signal Step 5.3's real client will raise for all of those.
        let intent = interpret(Some(&Failing), "type:jwt", 1_000_000, &no_projects);
        assert_eq!(intent, search::parse_intent("type:jwt"));
    }

    #[test]
    fn valid_llm_output_and_equivalent_cli_syntax_agree() {
        // Both sides resolve "today" against the real clock, so compare
        // against a window captured around the calls rather than a fixed
        // `now` — same technique as
        // `intent_schema::tests::time_tokens_match_the_cli_parser_for_shared_labels`.
        let llm = Fixed(r#"{"version":1,"text":"react","sources":["clipboard"],"time":"today"}"#);
        let before = crate::now_secs();
        let via_llm = interpret(Some(&llm), "irrelevant raw text", before, &no_projects);
        let via_cli = search::parse_intent("react today");
        let after = crate::now_secs();

        // clipboard-only isn't something the CLI syntax can express without
        // type:, so compare only the fields both paths actually agree on:
        // the resolved time window.
        let cli_since = via_cli.since.expect("today always sets since");
        assert!(cli_since >= before - 86_400 && cli_since <= after - 86_400);
        assert_eq!(via_llm.since, Some(before - 86_400));
        assert_eq!(via_llm.until, via_cli.until);
    }

    #[test]
    fn unresolvable_project_name_from_llm_does_not_error_the_whole_intent() {
        let llm = Fixed(r#"{"version":1,"project":"no such project"}"#);
        let intent = interpret(Some(&llm), "q", 1_000_000, &no_projects);
        assert_eq!(intent.project_id, None);
        assert!(!intent.object_types.is_empty());
    }
}
