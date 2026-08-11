//! The versioned JSON schema an LLM fills in to produce a [`SearchIntent`]
//! — the single, entire contract of the LLM boundary
//! (`ARCHITECTURE_INVARIANTS.md` §5.2).
//!
//! # What this module is for
//!
//! §5.1 forbids the LLM from touching the filesystem, a database, SQL, or
//! raw payloads. §5.2 narrows its whole job to "fill in this schema". This
//! module *is* that schema, plus the deterministic conversion from it to a
//! [`SearchIntent`]. Nothing here talks to a model, or to storage, or to
//! the network — it is pure data and pure functions, so the boundary can be
//! tested exhaustively without Ollama running.
//!
//! # Why the model cannot emit timestamps or ids
//!
//! [`SearchIntent`]'s own contract (rule 1) requires every field except
//! `text` to arrive already *resolved* — real epochs, real ids. That is
//! exactly what an LLM is worst at: it cannot know "now", and asking it for
//! an epoch invites confidently-wrong arithmetic. So the schema does not
//! contain a single timestamp or id field. Instead:
//!
//! - time is a [`TimeToken`] from a **closed set**, resolved against a
//!   caller-supplied `now` by [`TimeToken::resolve`];
//! - a project is a **name**, resolved to an id by a caller-supplied
//!   lookup — which keeps the database read on the deterministic side of
//!   §5.1, where it belongs.
//!
//! `#[serde(deny_unknown_fields)]` makes this structural rather than a
//! convention: a model that invents `"since": 1786451940` gets its whole
//! response rejected, instead of having the stray field silently ignored.
//!
//! # Naming
//!
//! The schema deliberately avoids the word "type" in every field name.
//! `ObjectType` (which *source*) and `detected_type` (which *content
//! classification*) are unrelated axes that already get confused in prose;
//! an LLM sees "type" as the natural name for a polymorphic discriminator
//! and would conflate them. They are `sources` and `content_kind` here.
//! This renaming is confined to the model-facing schema — the Rust field
//! names are unchanged (see `LUMOS_STORY.md` §17 for why renaming those was
//! rejected).

use crate::object::ObjectType;
use crate::search::{all_object_types, SearchIntent};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Bumped whenever the schema's meaning changes in a way an already-written
/// prompt could get wrong. A response carrying any other value is rejected
/// outright rather than best-effort parsed — a prompt and a parser that
/// disagree about the schema is precisely the situation where quietly
/// guessing produces confident nonsense.
pub const SCHEMA_VERSION: u32 = 1;

/// The sources a query may be restricted to, in model-facing spelling.
///
/// **Only sources that actually have storage appear here**, and that is a
/// correctness requirement, not tidiness: `retrieve_candidates` propagates
/// an error for any `ObjectType` the store can't serve, so a model
/// answering "screenshots" for an unimplemented source would fail the
/// entire search rather than return fewer results. Keeping the offered set
/// equal to the searchable set makes that unrepresentable.
/// `source_name_set_matches_searchable_types` locks the two together so a
/// future source can't be added to one and forgotten in the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceName {
    File,
    Clipboard,
}

impl SourceName {
    pub fn to_object_type(self) -> ObjectType {
        match self {
            SourceName::File => ObjectType::File,
            SourceName::Clipboard => ObjectType::Clipboard,
        }
    }
}

/// The closed set of time expressions the model may use.
///
/// Closed on purpose: this is the "fills a schema, does not generate a
/// query language" rule (§5.2) applied to time. The model picks a label;
/// deterministic code does every piece of date arithmetic.
///
/// Windows are rolling offsets from `now`, not calendar boundaries —
/// matching `search::parse_filters`'s existing, documented choice (exact
/// midnight-to-midnight needs timezone handling this codebase doesn't
/// have). `Today`/`Yesterday`/`LastWeek` resolve identically to the CLI
/// parser's `today`/`yesterday`/`week`, so both producers of a
/// `SearchIntent` agree — asserted by
/// `time_tokens_match_the_cli_parser_for_shared_labels`.
///
/// Calendar-anchored expressions ("between Monday and Friday", "after 10
/// August") are deliberately **absent** from v1: they need the timezone
/// handling just mentioned, and inventing them here would mean the schema
/// promises something the resolver cannot honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeToken {
    Today,
    Yesterday,
    LastWeek,
    LastMonth,
}

const DAY: i64 = 86_400;

impl TimeToken {
    /// Returns `(since, until)` as inclusive bounds, either of which may be
    /// `None`. `Yesterday` is the only token with a genuine upper bound —
    /// the others are open-ended up to now, where an explicit `until` of
    /// "now" would add nothing.
    pub fn resolve(self, now: i64) -> (Option<i64>, Option<i64>) {
        match self {
            TimeToken::Today => (Some(now - DAY), None),
            TimeToken::Yesterday => (Some(now - 2 * DAY), Some(now - DAY)),
            TimeToken::LastWeek => (Some(now - 7 * DAY), None),
            TimeToken::LastMonth => (Some(now - 30 * DAY), None),
        }
    }
}

/// The JSON object the model returns. Every field except `version` is
/// optional — an omitted field means "no opinion", which is a normal and
/// common answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentSchema {
    pub version: u32,
    /// Free text for FTS/semantic retrieval. The one field allowed to
    /// carry raw natural language, because that is what it feeds into.
    #[serde(default)]
    pub text: String,
    /// Which sources to search. Empty or omitted means "everywhere" and is
    /// resolved to [`all_object_types`] — never left empty, which
    /// `search_all` treats as a contract violation.
    #[serde(default)]
    pub sources: Vec<SourceName>,
    /// Content classification *within* a source — `"jwt"`, `"sql"`, … .
    /// Maps to `SearchIntent::detected_type`. Free-form on purpose: the
    /// classifier's vocabulary lives in `clipboard::classify` and changes
    /// independently of this schema, and a value matching nothing simply
    /// returns no results, which is honest.
    #[serde(default)]
    pub content_kind: Option<String>,
    #[serde(default)]
    pub time: Option<TimeToken>,
    /// A project *name*. Resolved to an id by the caller's lookup — the
    /// model never sees or invents an id (§5.1).
    #[serde(default)]
    pub project: Option<String>,
}

/// Parses and validates a model response. Rejects anything malformed, any
/// unknown schema version, any unknown source or time label, and any field
/// the schema doesn't define.
///
/// Every rejection is an `Err`, never a silently-defaulted value: the
/// intended caller (Step 5.3) falls back to the deterministic CLI parser on
/// error, so a loud failure degrades to working search, while a quiet one
/// would degrade to a confidently wrong query.
pub fn parse_schema_json(json: &str) -> Result<IntentSchema> {
    let schema: IntentSchema = serde_json::from_str(json)?;
    if schema.version != SCHEMA_VERSION {
        bail!(
            "unsupported intent schema version {} (this build understands {})",
            schema.version,
            SCHEMA_VERSION
        );
    }
    Ok(schema)
}

impl IntentSchema {
    /// Deterministic conversion to the engine's [`SearchIntent`].
    ///
    /// Infallible by construction — every way to be invalid is already
    /// rejected by [`parse_schema_json`], so anything holding an
    /// `IntentSchema` holds a valid one.
    ///
    /// `now` is passed in rather than read from the clock so time
    /// resolution is testable and reproducible. `resolve_project` is a
    /// caller-supplied lookup rather than a storage dependency, which keeps
    /// this module free of any database access at all; a name that doesn't
    /// resolve yields `None`, which is safe because `project_id` is a
    /// *ranking boost*, not a hard filter (verified by
    /// `search::tests::project_filtering_behavior_is_unchanged_by_phase_5`)
    /// — an unknown project name therefore degrades ranking slightly
    /// rather than wrongly excluding every result.
    pub fn into_intent(
        self,
        now: i64,
        resolve_project: impl Fn(&str) -> Option<i64>,
    ) -> SearchIntent {
        let (since, until) = match self.time {
            Some(token) => token.resolve(now),
            None => (None, None),
        };
        let object_types = if self.sources.is_empty() {
            all_object_types()
        } else {
            self.sources.into_iter().map(SourceName::to_object_type).collect()
        };
        SearchIntent {
            text: self.text,
            object_types,
            detected_type: self.content_kind,
            since,
            until,
            project_id: self.project.as_deref().and_then(resolve_project),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_projects(_: &str) -> Option<i64> {
        None
    }

    // ---- Structural guards ----

    /// The model-facing source list and the engine's searchable-type list
    /// must not drift. If a future source becomes searchable, this fails
    /// until it is also offered to the model (and vice versa) — the
    /// "unsupported source errors the whole search" trap made explicit.
    #[test]
    fn source_name_set_matches_searchable_types() {
        let offered: std::collections::HashSet<ObjectType> =
            [SourceName::File, SourceName::Clipboard]
                .into_iter()
                .map(SourceName::to_object_type)
                .collect();
        let searchable: std::collections::HashSet<ObjectType> =
            all_object_types().into_iter().collect();
        assert_eq!(
            offered, searchable,
            "every source offered to the model must be searchable, and vice versa"
        );
    }

    /// The schema must give a model no way to supply a raw timestamp or id.
    /// `deny_unknown_fields` is what enforces it; this proves the guard is
    /// actually on, rather than trusting that it was configured.
    #[test]
    fn raw_timestamps_and_ids_are_structurally_unrepresentable() {
        for injected in [
            r#"{"version":1,"since":1786451940}"#,
            r#"{"version":1,"until":1786451940}"#,
            r#"{"version":1,"project_id":7}"#,
            r#"{"version":1,"object_types":["file"]}"#,
            r#"{"version":1,"detected_type":"jwt"}"#,
        ] {
            assert!(
                parse_schema_json(injected).is_err(),
                "must reject out-of-schema field: {injected}"
            );
        }
    }

    // ---- Validation ----

    #[test]
    fn valid_response_parses_and_converts() {
        let json = r#"{
            "version": 1,
            "text": "authentication",
            "sources": ["clipboard"],
            "content_kind": "jwt",
            "time": "yesterday",
            "project": "CCM"
        }"#;
        let schema = parse_schema_json(json).unwrap();
        let intent = schema.into_intent(1_000_000, |name| (name == "CCM").then_some(42));

        assert_eq!(intent.text, "authentication");
        assert_eq!(intent.object_types, vec![ObjectType::Clipboard]);
        assert_eq!(intent.detected_type, Some("jwt".into()));
        assert_eq!(intent.since, Some(1_000_000 - 2 * DAY));
        assert_eq!(intent.until, Some(1_000_000 - DAY));
        assert_eq!(intent.project_id, Some(42));
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        assert!(parse_schema_json(r#"{"version":2}"#).is_err());
        assert!(parse_schema_json(r#"{"version":0}"#).is_err());
    }

    #[test]
    fn missing_version_is_rejected() {
        assert!(parse_schema_json(r#"{"text":"hello"}"#).is_err());
    }

    #[test]
    fn unknown_source_is_rejected_rather_than_skipped() {
        // Silently dropping it would turn "search my screenshots" into
        // "search everything", which is a different query.
        assert!(parse_schema_json(r#"{"version":1,"sources":["screenshot"]}"#).is_err());
        assert!(parse_schema_json(r#"{"version":1,"sources":["email"]}"#).is_err());
        assert!(parse_schema_json(r#"{"version":1,"sources":["nonsense"]}"#).is_err());
    }

    #[test]
    fn unknown_time_token_is_rejected() {
        assert!(parse_schema_json(r#"{"version":1,"time":"last_tuesday"}"#).is_err());
        assert!(parse_schema_json(r#"{"version":1,"time":"2026-08-11"}"#).is_err());
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(parse_schema_json("not json").is_err());
        assert!(parse_schema_json("").is_err());
        assert!(parse_schema_json("{").is_err());
    }

    // ---- Conversion ----

    #[test]
    fn omitted_sources_mean_everywhere_never_empty() {
        // The empty-`object_types` footgun: `search_all` rejects an empty
        // list, so the conversion must never produce one.
        for json in [r#"{"version":1}"#, r#"{"version":1,"sources":[]}"#] {
            let intent = parse_schema_json(json).unwrap().into_intent(0, no_projects);
            assert!(!intent.object_types.is_empty(), "for {json}");
            assert_eq!(intent.object_types, all_object_types(), "for {json}");
        }
    }

    #[test]
    fn a_bare_version_only_response_is_valid_and_neutral() {
        let intent = parse_schema_json(r#"{"version":1}"#).unwrap().into_intent(500, no_projects);
        assert_eq!(intent.text, "");
        assert_eq!(intent.detected_type, None);
        assert_eq!(intent.since, None);
        assert_eq!(intent.until, None);
        assert_eq!(intent.project_id, None);
    }

    #[test]
    fn unresolvable_project_name_yields_none_rather_than_failing() {
        let intent = parse_schema_json(r#"{"version":1,"project":"does not exist"}"#)
            .unwrap()
            .into_intent(0, no_projects);
        assert_eq!(intent.project_id, None);
    }

    #[test]
    fn time_tokens_resolve_to_expected_bounds() {
        let now = 10_000_000;
        assert_eq!(TimeToken::Today.resolve(now), (Some(now - DAY), None));
        assert_eq!(
            TimeToken::Yesterday.resolve(now),
            (Some(now - 2 * DAY), Some(now - DAY))
        );
        assert_eq!(TimeToken::LastWeek.resolve(now), (Some(now - 7 * DAY), None));
        assert_eq!(TimeToken::LastMonth.resolve(now), (Some(now - 30 * DAY), None));
    }

    #[test]
    fn only_yesterday_has_an_upper_bound() {
        let now = 10_000_000;
        for token in [TimeToken::Today, TimeToken::LastWeek, TimeToken::LastMonth] {
            assert_eq!(token.resolve(now).1, None, "{token:?} must be open-ended");
        }
        assert!(TimeToken::Yesterday.resolve(now).1.is_some());
    }

    /// Both producers of a `SearchIntent` — this schema and the CLI's
    /// `parse_intent` — must agree on the labels they share, or the same
    /// question asked two ways would return different answers.
    #[test]
    fn time_tokens_match_the_cli_parser_for_shared_labels() {
        // `parse_intent` reads the real clock, so compare against a window
        // captured around the call rather than a single instant.
        for (token, cli) in [
            (TimeToken::Today, "today"),
            (TimeToken::Yesterday, "yesterday"),
            (TimeToken::LastWeek, "week"),
        ] {
            let before = crate::now_secs();
            let cli_intent = crate::search::parse_intent(cli);
            let after = crate::now_secs();

            let (since_lo, until_lo) = token.resolve(before);
            let (since_hi, until_hi) = token.resolve(after);

            let cli_since = cli_intent.since.expect("shared labels all set `since`");
            assert!(
                cli_since >= since_lo.unwrap() && cli_since <= since_hi.unwrap(),
                "{cli}: schema and CLI disagree on `since`"
            );
            assert_eq!(
                cli_intent.until.is_some(),
                until_lo.is_some() && until_hi.is_some(),
                "{cli}: schema and CLI disagree on whether there is an upper bound"
            );
            if let (Some(cli_until), Some(lo), Some(hi)) = (cli_intent.until, until_lo, until_hi) {
                assert!(cli_until >= lo && cli_until <= hi, "{cli}: `until` disagrees");
            }
        }
    }

    #[test]
    fn conversion_is_pure_with_respect_to_now() {
        // Same schema, two different `now` values -> time bounds shift by
        // exactly the difference. Proves nothing reads a hidden clock.
        let mk = || parse_schema_json(r#"{"version":1,"time":"today"}"#).unwrap();
        let a = mk().into_intent(1_000, no_projects);
        let b = mk().into_intent(2_000, no_projects);
        assert_eq!(b.since.unwrap() - a.since.unwrap(), 1_000);
    }
}
