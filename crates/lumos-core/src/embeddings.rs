//! Semantic search — turns text into a vector so "ошибка базы" can find
//! "cannot connect to database" even though they share zero tokens, which
//! is exactly what FTS5 can never do.
//!
//! Model: `intfloat/multilingual-e5-small` (quantized ONNX export from the
//! `Xenova` mirror) — multilingual specifically because this user's queries
//! and copied text are a mix of Russian and English, and an English-only
//! model (e.g. plain MiniLM) would silently degrade for half of that.
//! ~118MB, downloaded once on first use and cached under
//! `%LOCALAPPDATA%\Lumos\models` — never bundled into the repo or the
//! installer.
//!
//! Storage/search is deliberately *not* sqlite-vec: at personal-history
//! scale (thousands, not millions, of rows) a brute-force cosine scan over
//! an in-memory `Vec<f32>` is a few milliseconds and requires no native
//! extension to load, build, or version-match across platforms. If this
//! ever needs to scale past ~100k rows, that's the point to revisit.
//!
//! Everything here degrades gracefully: no network on first run, or any
//! load/inference failure, just means semantic search silently doesn't
//! activate — FTS5 keeps working on its own, per the "works without an
//! LLM/network" principle the rest of Lumos already follows.

use crate::object::{ObjectId, ObjectType};
use crate::storage::EmbeddingStore;
use anyhow::{Context, Result};
use ndarray::Axis;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokenizers::{PaddingParams, Tokenizer};

const MODEL_URL: &str =
    "https://huggingface.co/Xenova/multilingual-e5-small/resolve/main/onnx/model_quantized.onnx";
const TOKENIZER_URL: &str =
    "https://huggingface.co/Xenova/multilingual-e5-small/resolve/main/tokenizer.json";
pub const DIMENSIONS: usize = 384;

fn model_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Lumos").join("models").join("multilingual-e5-small")
}

fn download_to(url: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let tmp = dest.with_extension("part");
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(&tmp)?;
    std::io::copy(&mut reader, &mut file).with_context(|| format!("downloading {url}"))?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

fn ensure_downloaded() -> Result<(PathBuf, PathBuf)> {
    let dir = model_dir();
    let model_path = dir.join("model.onnx");
    let tokenizer_path = dir.join("tokenizer.json");

    if !model_path.exists() {
        download_to(MODEL_URL, &model_path)?;
    }
    if !tokenizer_path.exists() {
        download_to(TOKENIZER_URL, &tokenizer_path)?;
    }
    Ok((model_path, tokenizer_path))
}

struct Embedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}

impl Embedder {
    fn load() -> Result<Self> {
        let (model_path, tokenizer_path) = ensure_downloaded()?;

        // ort's error type carries raw pointers internally and isn't
        // Send+Sync, so anyhow's blanket `?` conversion (which requires
        // that) doesn't apply — route every ort call through its Display
        // impl instead of the Error trait.
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("creating session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level1)
            .map_err(|e| anyhow::anyhow!("setting optimization level: {e}"))?
            .commit_from_file(&model_path)
            .map_err(|e| anyhow::anyhow!("loading ONNX model from {model_path:?}: {e}"))?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading tokenizer from {tokenizer_path:?}: {e}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));
        // The model's position embeddings only go up to 512 — a long
        // clipboard paste (up to 200KB of text) tokenizes to well past
        // that, and without truncation ort throws a broadcast-shape error
        // rather than anything resembling a helpful message about why.
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configuring tokenizer truncation: {e}"))?;

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    /// E5 models are trained with an instruction prefix distinguishing
    /// "things you search with" from "things you find" — using the wrong
    /// one measurably hurts retrieval quality, so this isn't optional.
    fn embed(&self, texts: &[&str], is_query: bool) -> Result<Vec<Vec<f32>>> {
        let prefix = if is_query { "query: " } else { "passage: " };
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();

        let encodings = self
            .tokenizer
            .encode_batch(prefixed.iter().map(String::as_str).collect::<Vec<_>>(), true)
            .map_err(|e| anyhow::anyhow!("tokenizing: {e}"))?;

        let batch = encodings.len();
        let seq_len = encodings.first().map(|e| e.len()).unwrap_or(0);
        if batch == 0 || seq_len == 0 {
            return Ok(Vec::new());
        }

        let ids: Vec<i64> = encodings.iter().flat_map(|e| e.get_ids().iter().map(|&i| i as i64)).collect();
        let mask: Vec<i64> = encodings
            .iter()
            .flat_map(|e| e.get_attention_mask().iter().map(|&i| i as i64))
            .collect();
        // BERT's segment-id input — always 0 for us since we only ever
        // encode a single sequence per input, never a sentence pair.
        let type_ids: Vec<i64> = vec![0i64; batch * seq_len];

        let ids_tensor = TensorRef::from_array_view(([batch, seq_len], &*ids))
            .map_err(|e| anyhow::anyhow!("building input_ids tensor: {e}"))?;
        let mask_tensor = TensorRef::from_array_view(([batch, seq_len], &*mask))
            .map_err(|e| anyhow::anyhow!("building attention_mask tensor: {e}"))?;
        let type_ids_tensor = TensorRef::from_array_view(([batch, seq_len], &*type_ids))
            .map_err(|e| anyhow::anyhow!("building token_type_ids tensor: {e}"))?;

        let mut session = self.session.lock().map_err(|_| anyhow::anyhow!("embedder session lock poisoned"))?;
        let outputs = session
            .run(ort::inputs![ids_tensor, mask_tensor, type_ids_tensor])
            .map_err(|e| anyhow::anyhow!("running inference: {e}"))?;

        // `last_hidden_state` — [batch, seq_len, hidden]. E5's own recipe is
        // mean pooling over real (non-padding) tokens, then L2-normalize so
        // dot product doubles as cosine similarity everywhere downstream.
        let hidden = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| anyhow::anyhow!("extracting output tensor: {e}"))?;
        let hidden = hidden
            .into_dimensionality::<ndarray::Ix3>()
            .context("expected [batch, seq, hidden] output from the embedding model")?;

        let mut result = Vec::with_capacity(batch);
        for b in 0..batch {
            let mut pooled = vec![0f32; hidden.shape()[2]];
            let mut count = 0f32;
            for s in 0..seq_len {
                if mask[b * seq_len + s] == 0 {
                    continue;
                }
                count += 1.0;
                let batch_slice = hidden.index_axis(Axis(0), b);
                let row = batch_slice.index_axis(Axis(0), s);
                for (p, v) in pooled.iter_mut().zip(row.iter()) {
                    *p += v;
                }
            }
            if count > 0.0 {
                for p in pooled.iter_mut() {
                    *p /= count;
                }
            }
            let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
            for p in pooled.iter_mut() {
                *p /= norm;
            }
            result.push(pooled);
        }
        Ok(result)
    }
}

static EMBEDDER: OnceLock<Option<Embedder>> = OnceLock::new();

fn embedder() -> Option<&'static Embedder> {
    EMBEDDER
        .get_or_init(|| match Embedder::load() {
            Ok(e) => Some(e),
            Err(e) => {
                // No network on first run, disk full, whatever — semantic
                // search just doesn't activate this session. Not fatal.
                log::warn!("semantic search unavailable: {e}");
                None
            }
        })
        .as_ref()
}

pub fn is_available() -> bool {
    embedder().is_some()
}

// Vector <-> byte encoding used to live here; it moved to the SQLite store
// in Phase 2. How a vector is serialized is a storage concern — this module
// now only ever sees `&[f32]`.

/// Embeds and stores `text` for `id`, replacing any existing vector.
/// No-op (returns `Ok(())`) if the model isn't available — callers don't
/// need their own fallback branch.
pub fn store(store: &impl EmbeddingStore, id: ObjectId, text: &str) -> Result<()> {
    let Some(embedder) = embedder() else { return Ok(()) };
    let vectors = embedder.embed(&[text], false)?;
    let Some(vector) = vectors.into_iter().next() else { return Ok(()) };
    store.put_vector(id, &vector)
}

/// Catches up anything that slipped through — a row inserted while the
/// model was still downloading on first run, one from before this feature
/// existed, whatever. Both the CLI (`lumos embed`) and the app's own
/// startup pass call this; it's the one place that work is defined.
/// Returns `(files_embedded, clipboard_entries_embedded)`.
pub fn backfill_all(st: &impl EmbeddingStore) -> Result<(usize, usize)> {
    if !is_available() {
        return Ok((0, 0));
    }

    let mut counts = [0usize; 2];
    for (slot, object_type) in [ObjectType::File, ObjectType::Clipboard].into_iter().enumerate() {
        // One query returns ids *and* their text, instead of a lookup per
        // row as the pre-2.1 version did.
        let pending = st.pending_embeddings(object_type, usize::MAX)?;
        for (id, text) in &pending {
            store(st, *id, text)?;
        }
        counts[slot] = pending.len();
    }

    Ok((counts[0], counts[1]))
}

/// How far below the best match's score a candidate may fall and still be
/// considered relevant, rather than padding.
///
/// **Why relative to the top score, not an absolute cutoff.** Measured
/// directly against real production data (`multilingual-e5-small`, this
/// crate's actual model): cosine similarity for short, non-natural-language
/// strings — tokens, filenames, UUIDs, JSON fragments, exactly the bulk of
/// what Lumos indexes — sits in a narrow, anisotropic band regardless of
/// query. A query with **zero** genuinely related content in the corpus
/// ("recipe for chicken soup" against a clipboard history full of tokens
/// and stack traces) still scored 0.79–0.81 against everything; a query
/// with genuine matches ("jwt") scored top candidates at 0.82–0.84 —
/// *overlapping* the no-match baseline, not clearly separated from it. An
/// absolute threshold cannot thread that needle: set it above ~0.81 and a
/// no-match query still returns nothing useful anyway so the floor doesn't
/// matter, but set it below ~0.84 and it admits most of the no-match
/// baseline too. There is no single number that is simultaneously "above
/// the noise floor" and "below genuine matches," because the two overlap.
///
/// A margin relative to *that query's own best score* sidesteps this: it
/// doesn't ask "is this an absolutely good match" (unanswerable given the
/// overlap above), only "is this meaningfully worse than the best thing
/// this query found" — which adapts automatically to wherever that
/// query's baseline happens to sit, without needing to know it in advance.
///
/// **Measured basis for the value.** Across the genuine-match cases
/// checked (a `jwt` query against real JWT-classified clipboard entries;
/// a `sql` query against a real SQL clipboard entry), the true match sat
/// 0.017–0.027 below that query's top score. `0.03` is set just above
/// that observed range — a first-pass empirical constant from this one
/// investigation, not a tuned final value, same honesty as this crate's
/// other ranking constants (see `search.rs`'s `RRF_K`/boost weights).
///
/// **What this does not fix, disclosed rather than hidden:** it only ever
/// removes a *trailing* suffix of an already-sorted list — it cannot
/// promote a genuine match that scores *below* unrelated noise, which the
/// same investigation also observed happening (a real SQL entry ranked
/// below several unrelated clipboard entries for the query `sql`). That is
/// a ranking/representation problem, not a padding problem, and is not
/// solved here. See `ARCHITECTURE_INVARIANTS.md`'s Phase 4 entry.
const RELEVANCE_MARGIN: f32 = 0.03;

/// The fraction of the corpus that may sit within [`RELEVANCE_MARGIN`] of
/// the top score before the whole ranking is judged uninformative.
///
/// **Why concentration, not an absolute score threshold.** Measured on the
/// real model against the real corpus, these two queries are essentially
/// indistinguishable by score yet opposite in relevance:
///
/// ```text
/// "jwt"                               top=0.8579  band=652/4953 (13.2%)  -> noise ("jp.png")
/// "database connection configuration" top=0.8578  band= 56/4953 ( 1.1%)  -> genuine ("IdentityServerConfigurationDbContext.cs")
/// ```
///
/// Their top scores differ by 0.0001. No absolute cutoff can separate
/// them. What *does* separate them is the shape of the distribution
/// underneath the top: a genuine match is a sharp peak standing clear of
/// the corpus, while noise produces a flat plateau where the "best" result
/// is merely the maximum of a crowd that is all equally (ir)relevant. That
/// is exactly the signature of embedding anisotropy — absolute cosine
/// carries little information, but *relative concentration* carries a lot.
///
/// Measured separation across both corpora (files n=4953, clipboard n=99),
/// nine queries spanning short tokens, natural language, identifiers,
/// paths and a deliberate no-match control:
///
/// ```text
/// genuine relevance: 0.3% 0.4% 0.6% 1.0% 1.1% 1.3% 2.0% 3.0% 3.0%
/// noise            : 6.6% 13.1% 13.2% 17.8% 25.3% 26.3% 31.3% 37.4%
/// ```
///
/// A clean gap sits between 3.0% and 6.6%; 5% is placed in the middle of
/// it. Like this crate's other ranking constants this is an empirical
/// first-pass value, not a tuned optimum — but unlike a bare score
/// threshold it is measuring a property that actually distinguishes the
/// two cases.
const MAX_TOP_BAND_FRACTION: f32 = 0.05;

/// Corpora smaller than this are exempt from the concentration check —
/// "what fraction sits near the top" is not a meaningful statistic over a
/// handful of vectors, and applying it there would reject perfectly good
/// matches purely because the sample is tiny.
const MIN_CORPUS_FOR_CONCENTRATION: usize = 20;

/// Decides which of `hits` are genuinely relevant. `hits` must already be
/// sorted score-descending (as `EmbeddingStore::nearest` guarantees), and
/// `corpus_size` is the total number of vectors of this type.
///
/// Two independent rules, in order:
///
/// 1. **Concentration.** If more than [`MAX_TOP_BAND_FRACTION`] of the
///    corpus falls within [`RELEVANCE_MARGIN`] of the top score, the top
///    is not a genuine outlier — it is the maximum of a flat distribution.
///    The entire list is rejected. Returning nothing is the correct answer
///    for a query with no real match; padding it with the least-bad noise
///    is not.
/// 2. **Margin.** Otherwise, keep the leading run within
///    [`RELEVANCE_MARGIN`] of the top and drop the trailing tail.
///
/// Never reorders and never adds — only ever shortens. Pure, so the whole
/// judgment is testable without a real embedding model.
///
/// `hits` may be a truncated sample rather than the full corpus; the band
/// count is therefore a lower bound, which is sufficient because the rule
/// only needs to detect *exceeding* the threshold. `search` sizes its
/// sample so that a band filling the entire sample already exceeds it.
fn filter_by_relevance(hits: Vec<(i64, f32)>, corpus_size: usize) -> Vec<(i64, f32)> {
    let Some(&(_, top_score)) = hits.first() else {
        return hits;
    };
    let cutoff = top_score - RELEVANCE_MARGIN;
    let band = hits.iter().take_while(|&&(_, score)| score >= cutoff).count();

    if corpus_size >= MIN_CORPUS_FOR_CONCENTRATION
        && band as f32 > corpus_size as f32 * MAX_TOP_BAND_FRACTION
    {
        return Vec::new();
    }

    hits.into_iter().take(band).collect()
}

/// Brute-force cosine search (vectors are pre-normalized, so this is just a
/// dot product) — see the module doc for why this beats sqlite-vec at this
/// scale. Returns `(object_id, score)` sorted best-first, with the
/// [`RELEVANCE_MARGIN`] cutoff already applied — callers never have to
/// filter this themselves, and `EmbeddingStore::nearest()`'s own contract
/// (an index operation, no relevance judgment) stays unchanged.
///
/// This module owns turning text into a vector; finding the nearest stored
/// vectors is an index operation and lives behind `EmbeddingStore::nearest`
/// — the same side of the boundary as an FTS `MATCH`. That split is what
/// lets the brute-force scan be swapped for an ANN index without any caller
/// noticing. (Phase 2 had it the other way round and would have had to move
/// this code anyway.) The relevance margin is deliberately layered on top
/// of that call rather than pushed into `nearest()`/`storage_sqlite.rs`:
/// "how far is too far from the best match" is a judgment about relevance,
/// not about finding candidates, and keeping it here means the storage
/// trait's contract never had to change to fix this.
pub fn search(
    st: &impl EmbeddingStore,
    object_type: ObjectType,
    query: &str,
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    let Some(embedder) = embedder() else { return Ok(Vec::new()) };
    let query_vec = embedder
        .embed(&[query], true)?
        .into_iter()
        .next()
        .unwrap_or_default();
    if query_vec.is_empty() {
        return Ok(Vec::new());
    }

    // Fetch enough candidates to *judge* the distribution, not just enough
    // to return. The concentration rule needs to detect a band wider than
    // `MAX_TOP_BAND_FRACTION` of the corpus, so the sample is sized one
    // past that threshold: if every candidate in the sample sits inside
    // the margin, the real band already exceeds what's allowed and the
    // list is rejected without needing its exact size. Cheap in practice —
    // `nearest` already scores the whole corpus regardless of `limit`, so
    // this only widens the sort/truncate, not the scan.
    let corpus_size = st.vector_count(object_type)?;
    let allowed_band = (corpus_size as f32 * MAX_TOP_BAND_FRACTION).ceil() as usize;
    let sample_n = limit.max(allowed_band.saturating_add(1));

    let hits = st.nearest(object_type, &query_vec, sample_n)?;
    let mut relevant = filter_by_relevance(hits, corpus_size);
    relevant.truncate(limit);
    Ok(relevant)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Corpus big enough that the concentration rule is active, used
    /// wherever a test isn't specifically about the small-corpus exemption.
    const BIG: usize = 1000;

    #[test]
    fn relevant_candidates_within_margin_are_retained() {
        let hits = vec![(1, 0.90), (2, 0.88), (3, 0.875)];
        assert_eq!(
            filter_by_relevance(hits, BIG),
            vec![(1, 0.90), (2, 0.88), (3, 0.875)]
        );
    }

    #[test]
    fn weak_matches_beyond_margin_are_excluded() {
        let hits = vec![(1, 0.90), (2, 0.85), (3, 0.60), (4, 0.40)];
        // top=0.90, cutoff=0.87 -> only id 1 is inside the margin.
        assert_eq!(filter_by_relevance(hits, BIG), vec![(1, 0.90)]);
    }

    #[test]
    fn does_not_force_k_results_when_fewer_are_relevant() {
        let hits: Vec<(i64, f32)> = std::iter::once((1_i64, 0.90_f32))
            .chain((2..=10).map(|id| (id, 0.5 - id as f32 * 0.01)))
            .collect();
        assert_eq!(filter_by_relevance(hits, BIG), vec![(1, 0.90)]);
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(filter_by_relevance(Vec::new(), BIG), Vec::new());
    }

    #[test]
    fn relative_order_of_retained_candidates_is_unchanged() {
        let hits = vec![(3, 0.95), (1, 0.94), (2, 0.93), (9, 0.10)];
        assert_eq!(
            filter_by_relevance(hits, BIG),
            vec![(3, 0.95), (1, 0.94), (2, 0.93)]
        );
    }

    #[test]
    fn ties_at_the_top_are_all_retained() {
        let hits = vec![(1, 0.90), (2, 0.90), (3, 0.90), (4, 0.10)];
        assert_eq!(
            filter_by_relevance(hits, BIG),
            vec![(1, 0.90), (2, 0.90), (3, 0.90)]
        );
    }

    // ---- Concentration rule: the ranking-inversion fix ----

    /// The measured `"jwt"` reproduction, reduced to its numbers: a large
    /// corpus where a big fraction of it clusters within the margin of the
    /// top. The old margin-only logic returned the whole plateau (and with
    /// it `NotificationsTable.jsx`); the concentration rule rejects the
    /// list outright. This test fails against the pre-fix implementation.
    #[test]
    fn flat_distribution_is_rejected_entirely_as_uninformative() {
        // 200 candidates all within 0.03 of each other, corpus 1000 ->
        // 20% band, far past the 5% allowance.
        let hits: Vec<(i64, f32)> = (0..200).map(|i| (i, 0.858 - i as f32 * 0.0001)).collect();
        assert_eq!(
            filter_by_relevance(hits, 1000),
            Vec::new(),
            "a top score that is merely the maximum of a flat crowd is not a match"
        );
    }

    /// The measured `"database connection configuration"` case: virtually
    /// the same top score as the `jwt` noise case above (0.8578 vs 0.8579
    /// in the real measurement), but a sharp peak instead of a plateau.
    /// Score alone cannot tell these apart — concentration can.
    #[test]
    fn sharp_peak_is_kept_even_at_the_same_top_score_as_a_rejected_flat_one() {
        let hits: Vec<(i64, f32)> = (0..11).map(|i| (i, 0.8578 - i as f32 * 0.0025)).collect();
        let kept = filter_by_relevance(hits, 1000);
        assert!(!kept.is_empty(), "a genuine outlier must survive");
        assert_eq!(kept[0].0, 0);
        assert!(kept.len() <= 13, "only the peak, not a plateau");
    }

    #[test]
    fn small_corpora_are_exempt_from_the_concentration_rule() {
        // 3 of 4 vectors within the margin is 75% — way past the fraction
        // — but 4 vectors is far too few to infer a distribution from.
        let hits = vec![(1, 0.90), (2, 0.895), (3, 0.89)];
        assert_eq!(
            filter_by_relevance(hits, 4),
            vec![(1, 0.90), (2, 0.895), (3, 0.89)]
        );
    }

    #[test]
    fn concentration_rule_is_scale_relative_not_absolute() {
        // Identical hit list, identical scores; only the corpus size
        // differs. Proves the decision is about *fraction of corpus*, not
        // about any hardcoded score value — i.e. it cannot be
        // keyword- or magnitude-specific.
        let hits: Vec<(i64, f32)> = (0..30).map(|i| (i, 0.90 - i as f32 * 0.0005)).collect();
        assert!(
            filter_by_relevance(hits.clone(), 100).is_empty(),
            "30 of 100 is 30% — a plateau"
        );
        assert_eq!(
            filter_by_relevance(hits, 10_000).len(),
            30,
            "the same 30 out of 10000 is 0.3% — a genuine peak"
        );
    }

    #[test]
    fn identical_scores_are_judged_only_by_concentration_never_by_magnitude() {
        // Same top score (0.99, very "high"), same shape, two corpus
        // sizes. A high absolute score does not rescue a plateau, and a
        // low one does not condemn a peak — there is no magnitude test
        // anywhere in the rule.
        let plateau: Vec<(i64, f32)> = (0..40).map(|i| (i, 0.99 - i as f32 * 0.0001)).collect();
        assert!(filter_by_relevance(plateau, 200).is_empty());

        let low_peak = vec![(1_i64, 0.31_f32), (2, 0.30)];
        assert_eq!(filter_by_relevance(low_peak, 200).len(), 2);
    }
}

