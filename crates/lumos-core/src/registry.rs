//! Phase 6, Step 6.2 — the one authoritative list of `ObjectType`s Lumos
//! has a real, working source for.
//!
//! Before this module, `search::all_object_types()` and
//! `storage_sqlite::SUPPORTED` each hand-maintained their own copy of this
//! exact list — two independent exhaustive matches that happened to agree
//! because nobody had yet let them drift, not because anything enforced
//! agreement. This module is that enforcement: both now read the same
//! constant.
//!
//! This is deliberately *not* a trait, not a plugin system, not runtime
//! registration. Nothing about File or Clipboard varies in a way a trait
//! method would express — both already go through identical generic calls
//! (`FullTextIndex`, `EmbeddingStore`, `ObjectStore`) dispatched purely on
//! `ObjectType`. The only fact duplicated between search and storage was
//! "which types are those" — a plain list is the smallest thing that can
//! hold one fact. See `ARCHITECTURE_INVARIANTS.md`'s Phase 6 entry and
//! `LUMOS_STORY.md` for why a heavier abstraction was considered and
//! rejected as premature.

use crate::object::ObjectType;

/// Every `ObjectType` Lumos can actually search and persist today, in
/// declaration order. Adding a real third source is exactly one line here
/// — not a synchronized edit to `search.rs` and `storage_sqlite.rs`
/// separately.
///
/// `ObjectType` declares six variants; only two are registered. The other
/// four (`Screenshot`, `BrowserVisit`, `Session`, `Email`) exist as
/// vocabulary (`ARCHITECTURE_INVARIANTS.md` §1) with no ingestion pipeline
/// behind them yet — deliberately absent, not forgotten, per
/// `is_deliberately_classified` below.
pub const REGISTERED_OBJECT_TYPES: &[ObjectType] = &[ObjectType::File, ObjectType::Clipboard];

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain `const` array cannot fail to compile when a new
    /// `ObjectType` variant is added and left unclassified — unlike the
    /// exhaustive `match` this module replaces (in `search.rs` and
    /// `storage_sqlite.rs`), which used to fail to compile in that case.
    /// This test restores that guarantee at the test layer, the same
    /// pattern already used for `intent_schema::SourceName` vs
    /// `search::all_object_types()` in Phase 5.1
    /// (`source_name_set_matches_searchable_types`): an exhaustive match
    /// here forces this test to be touched — and to fail if it isn't —
    /// the moment a new variant is declared, before that variant can
    /// silently fall through as "not registered" by omission.
    #[test]
    fn every_object_type_is_deliberately_classified() {
        fn is_registered(t: ObjectType) -> bool {
            match t {
                ObjectType::File | ObjectType::Clipboard => true,
                ObjectType::Screenshot
                | ObjectType::BrowserVisit
                | ObjectType::Session
                | ObjectType::Email => false,
            }
        }

        for &t in REGISTERED_OBJECT_TYPES {
            assert!(is_registered(t), "{t:?} is in REGISTERED_OBJECT_TYPES but not classified as registered above");
        }
        let registered_count = [
            ObjectType::File,
            ObjectType::Clipboard,
            ObjectType::Screenshot,
            ObjectType::BrowserVisit,
            ObjectType::Session,
            ObjectType::Email,
        ]
        .into_iter()
        .filter(|&t| is_registered(t))
        .count();
        assert_eq!(
            registered_count,
            REGISTERED_OBJECT_TYPES.len(),
            "every type classified `true` above must actually appear in REGISTERED_OBJECT_TYPES, and vice versa"
        );
    }
}
