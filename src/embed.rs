//! Static (model2vec-style) sentence embeddings with no ML runtime.
//!
//! A sentence vector is the mean of per-token rows from one matrix, so
//! inference is tokenisation plus a few hundred additions: microseconds on a
//! laptop, well under a millisecond on a Raspberry Pi. Weights are stored as
//! int8 with a per-row scale, so the bundled model is ~4x smaller than f32
//! and only the rows actually touched are paged into memory.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::Result;

const MAGIC: &[u8; 4] = b"RV01";
const MAX_TOKENS: usize = 512;
const MAX_WORD_CHARS: usize = 100;

pub struct Embedder {
    vocab: HashMap<String, u32>,
    unk: Option<u32>,
    dim: usize,
    scales: Vec<f32>,
    rows: Rows,
}

enum Rows {
    F32(Vec<f32>),
    I8(&'static [u8]),
    I8Owned(Vec<u8>),
}

impl Embedder {
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Load a model2vec directory (`model.safetensors` + `tokenizer.json`).
    pub fn from_model2vec_dir(dir: &Path) -> Result<Self> {
        let tok: Value = serde_json::from_slice(&std::fs::read(dir.join("tokenizer.json"))?)?;
        let model = &tok["model"];
        if model["type"] != "WordPiece" {
            return Err("only WordPiece tokenizers are supported".into());
        }
        let vocab: HashMap<String, u32> = model["vocab"]
            .as_object()
            .ok_or("tokenizer.json has no vocab")?
            .iter()
            .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0) as u32))
            .collect();
        let unk = model["unk_token"].as_str().and_then(|u| vocab.get(u).copied());

        let bytes = std::fs::read(dir.join("model.safetensors"))?;
        let n = u64::from_le_bytes(bytes[..8].try_into()?) as usize;
        let header: Value = serde_json::from_slice(&bytes[8..8 + n])?;
        let t = &header["embeddings"];
        if t["dtype"] != "F32" {
            return Err("expected F32 embeddings".into());
        }
        let shape: Vec<usize> = t["shape"].as_array().ok_or("bad shape")?.iter().filter_map(|v| v.as_u64()).map(|v| v as usize).collect();
        let off: Vec<usize> = t["data_offsets"].as_array().ok_or("bad offsets")?.iter().filter_map(|v| v.as_u64()).map(|v| v as usize).collect();
        let data = &bytes[8 + n + off[0]..8 + n + off[1]];
        let rows: Vec<f32> = data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        if rows.len() != shape[0] * shape[1] {
            return Err("embedding size mismatch".into());
        }
        Ok(Embedder { vocab, unk, dim: shape[1], scales: Vec::new(), rows: Rows::F32(rows) })
    }

    /// Serialise as an int8 bundle:
    /// `RV01 | dim u32 | n u32 | unk u32 | vocab_len u32 | vocab (\n-joined, by id) | scales f32×n | rows i8×n×dim`.
    pub fn to_bundle(&self) -> Vec<u8> {
        let n = self.vocab.len();
        let mut by_id = vec![""; n];
        for (k, &v) in &self.vocab {
            by_id[v as usize] = k;
        }
        let vocab = by_id.join("\n");
        let mut out = Vec::with_capacity(20 + vocab.len() + n * (4 + self.dim));
        out.extend_from_slice(MAGIC);
        for v in [self.dim as u32, n as u32, self.unk.unwrap_or(u32::MAX), vocab.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(vocab.as_bytes());
        let mut q = Vec::with_capacity(n * self.dim);
        for id in 0..n {
            let row = self.row_f32(id as u32);
            let max = row.iter().fold(0f32, |m, x| m.max(x.abs()));
            let scale = if max == 0.0 { 1.0 } else { max / 127.0 };
            out.extend_from_slice(&scale.to_le_bytes());
            q.extend(row.iter().map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8 as u8));
        }
        out.extend_from_slice(&q);
        out
    }

    /// Load an int8 bundle. With a `'static` slice (e.g. `include_bytes!`) the
    /// weight matrix is used in place and never copied.
    pub fn from_bundle(bytes: &'static [u8]) -> Result<Self> {
        let (mut e, off) = Self::parse_bundle(bytes)?;
        e.rows = Rows::I8(&bytes[off..]);
        Ok(e)
    }

    pub fn from_bundle_owned(mut bytes: Vec<u8>) -> Result<Self> {
        let (mut e, off) = Self::parse_bundle(&bytes)?;
        e.rows = Rows::I8Owned(bytes.split_off(off));
        Ok(e)
    }

    /// Parse everything but the weight matrix; returns the matrix offset.
    fn parse_bundle(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < 20 || &bytes[..4] != MAGIC {
            return Err("not a recall embedding bundle".into());
        }
        let u = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        let (dim, n, unk, vlen) = (u(4), u(8), u(12), u(16));
        let mut p = 20;
        let vocab_text = std::str::from_utf8(&bytes[p..p + vlen])?;
        p += vlen;
        let vocab: HashMap<String, u32> = vocab_text.split('\n').enumerate().map(|(i, t)| (t.to_string(), i as u32)).collect();
        let scales: Vec<f32> = bytes[p..p + 4 * n].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        p += 4 * n;
        if bytes.len() - p != n * dim {
            return Err("truncated embedding bundle".into());
        }
        let unk = (unk != u32::MAX as usize).then_some(unk as u32);
        Ok((Embedder { vocab, unk, dim, scales, rows: Rows::F32(Vec::new()) }, p))
    }

    fn row_f32(&self, id: u32) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        self.add_row(id, &mut v);
        v
    }

    fn add_row(&self, id: u32, acc: &mut [f32]) {
        let start = id as usize * self.dim;
        match &self.rows {
            Rows::F32(r) => acc.iter_mut().zip(&r[start..start + self.dim]).for_each(|(a, x)| *a += x),
            Rows::I8(r) => add_i8(acc, &r[start..start + self.dim], self.scales[id as usize]),
            Rows::I8Owned(r) => add_i8(acc, &r[start..start + self.dim], self.scales[id as usize]),
        }
    }

    /// Whether `word` is a whole token in the model vocabulary (i.e. a real
    /// word, as opposed to a typo that splits into sub-word pieces).
    pub fn knows(&self, word: &str) -> bool {
        self.vocab.contains_key(word)
    }

    /// Unit-length sentence embedding (all zeros if no known tokens).
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let ids = self.tokenize(text);
        let mut v = vec![0f32; self.dim];
        for &id in &ids {
            self.add_row(id, &mut v);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }

    /// BERT-style normalisation + pre-tokenisation + greedy WordPiece.
    /// Unknown tokens are dropped, as model2vec does.
    pub fn tokenize(&self, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        let mut piece = String::new();
        for word in pre_tokenize(text) {
            self.wordpiece(&word, &mut piece, &mut ids);
            if ids.len() >= MAX_TOKENS {
                ids.truncate(MAX_TOKENS);
                break;
            }
        }
        if let Some(unk) = self.unk {
            ids.retain(|&i| i != unk);
        }
        ids
    }

    fn wordpiece(&self, word: &str, buf: &mut String, out: &mut Vec<u32>) {
        if word.chars().count() > MAX_WORD_CHARS {
            return;
        }
        let mark = out.len();
        let mut start = 0;
        while start < word.len() {
            let mut end = word.len();
            let mut found = None;
            while end > start {
                buf.clear();
                if start > 0 {
                    buf.push_str("##");
                }
                buf.push_str(&word[start..end]);
                if let Some(&id) = self.vocab.get(buf.as_str()) {
                    found = Some(id);
                    break;
                }
                end = word[..end].char_indices().next_back().map_or(start, |(i, _)| i);
            }
            match found {
                Some(id) => {
                    out.push(id);
                    start = end;
                }
                None => {
                    // Whole word becomes [UNK], which is then dropped.
                    out.truncate(mark);
                    return;
                }
            }
        }
    }
}

fn add_i8(acc: &mut [f32], row: &[u8], scale: f32) {
    acc.iter_mut().zip(row).for_each(|(a, &x)| *a += (x as i8) as f32 * scale);
}

fn pre_tokenize(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, words: &mut Vec<String>| {
        if !cur.is_empty() {
            words.push(std::mem::take(cur));
        }
    };
    for c in text.nfd().filter(|c| !is_combining_mark(*c)) {
        if c == '\0' || c == '\u{FFFD}' || (c.is_control() && !c.is_whitespace()) {
            continue;
        }
        if c.is_whitespace() {
            flush(&mut cur, &mut words);
        } else if is_punct(c) || is_cjk(c) {
            flush(&mut cur, &mut words);
            words.push(c.to_lowercase().collect());
        } else {
            cur.extend(c.to_lowercase());
        }
    }
    flush(&mut cur, &mut words);
    words
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric() && !c.is_whitespace())
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0x2A700..=0x2B73F | 0x2B740..=0x2B81F | 0x2B820..=0x2CEAF | 0xF900..=0xFAFF | 0x2F800..=0x2FA1F)
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn from_bytes(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// The embedding model compiled into the binary, if built with `--features model`.
pub fn bundled() -> Option<&'static Embedder> {
    #[cfg(feature = "model")]
    {
        static MODEL: std::sync::OnceLock<Option<Embedder>> = std::sync::OnceLock::new();
        static BYTES: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/models/recall.rv"));
        MODEL.get_or_init(|| Embedder::from_bundle(BYTES).ok()).as_ref()
    }
    #[cfg(not(feature = "model"))]
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_tokenizes_like_bert() {
        assert_eq!(pre_tokenize("Café, Ines's   e-reader!"), ["cafe", ",", "ines", "'", "s", "e", "-", "reader", "!"]);
    }
}
