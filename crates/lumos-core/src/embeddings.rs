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
        let ids = st.ids_missing_vectors(object_type)?;
        for local_id in &ids {
            let id = ObjectId::new(object_type, *local_id);
            if let Some(text) = st.embeddable_text(id)? {
                store(st, id, &text)?;
            }
        }
        counts[slot] = ids.len();
    }

    Ok((counts[0], counts[1]))
}

/// Brute-force cosine search (vectors are pre-normalized, so this is just a
/// dot product) — see the module doc for why this beats sqlite-vec at this
/// scale. Returns `(object_id, score)` sorted best-first.
///
/// Storage supplies the vectors; the scoring, sorting and cutoff are domain
/// rules and stay here. Swapping the brute-force scan for an ANN index is
/// therefore a change to `load_vectors`, not to this function's callers.
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

    let mut scored: Vec<(i64, f32)> = st
        .load_vectors(object_type)?
        .into_iter()
        .map(|(id, v)| {
            let score: f32 = query_vec.iter().zip(v.iter()).map(|(a, b)| a * b).sum();
            (id, score)
        })
        .collect();

    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    Ok(scored)
}

