# Retrieval benchmark

Question: which retrieval strategy should a personal-memory server use, given
that it must be fast, tiny, and run on a Raspberry Pi?

## Dataset

Public memory benchmarks (LongMemEval, LoCoMo) test retrieval over *chat
transcripts*. Recall stores curated *facts* (like ChatGPT's saved memories), so
we built a fact-retrieval set that mirrors how agents actually query:

- [`data/facts.jsonl`](data/facts.jsonl): 78 facts about one fictional person
  (identity, work, preferences, family, hobbies, travel, instructions).
- [`data/queries.jsonl`](data/queries.jsonl): 124 queries with gold fact ids:
  - **keyword** (48): shares words with the fact — `"anniversary date"`
  - **paraphrase** (40): little or no word overlap — `"pet"` → greyhound, `"significant other"` → partner
  - **task** (24): the raw task an agent is doing — `"Plan a dinner party menu"` → vegetarian, peanut allergy, lactose intolerant, no alcohol, likes spicy food
  - **typo** (12): `"vegitarian"`, `"greyhund"`, `"mancester"`
- Distractors: 1,000 or 10,000 deterministic templated memories in the same
  style ("User read an article about volcanoes while visiting Lima in 2014.").

Metrics: Recall@5/@10 (fraction of gold facts retrieved), MRR, p50/p99
latency of a full `Store::search` call (tokenise, embed, query SQLite, fetch rows),
single-threaded on an Apple M-series laptop.

Caveats: one synthetic persona, labels written by hand, 124 queries — treat
differences under ~3 points as noise.

## Results (R@5)

| Strategy | 78 | 1,078 | 10,078 | p50 @1k | Size |
|---|---:|---:|---:|---:|---:|
| BM25, all terms (FTS5 default) | 0.190 | 0.190 | 0.190 | 28 µs | — |
| BM25, any term | 0.544 | 0.542 | 0.542 | 46 µs | — |
| Dense potion-base-2M | 0.647 | 0.530 | 0.468 | 18 µs | 2.2 MB |
| Dense potion-base-4M | 0.716 | 0.621 | 0.588 | 51 µs | 4.1 MB |
| Dense potion-base-8M | 0.803 | 0.701 | 0.666 | 110 µs | 7.9 MB |
| Dense potion-base-32M | 0.813 | 0.743 | 0.691 | 271 µs | 33 MB |
| Hybrid (BM25 + dense, RRF), potion-8M, no typo repair | 0.782 | 0.735 | 0.724 | 141 µs | 7.9 MB |
| **Hybrid + typo repair, potion-8M (shipped)** | **0.823** | **0.771** | **0.765** | **152 µs** | **7.9 MB** |
| Hybrid + typo repair, potion-32M | 0.815 | 0.770 | 0.778 | 396 µs | 33 MB |
| Dense bge-small-en-v1.5 (transformer, reference) | 0.822 | 0.781 | — | ms | 130 MB + ONNX |
| Dense all-MiniLM-L6-v2 (transformer, reference) | 0.848 | 0.760 | — | ms | 90 MB + ONNX |

By query type at 78 memories (shipped config): keyword 0.969, paraphrase
0.740, task 0.664, typo 0.833. Full tables: [`results.md`](results.md),
[`floor.md`](floor.md).

## Findings

1. **FTS5's default (all terms must match) is unusable** for agent queries
   (0.19). OR-of-terms BM25 nails keywords (0.97) but is blind to paraphrase (0.11).
2. **Static embeddings fix paraphrase** (0.11 → 0.79) at microsecond cost.
3. **Hybrid beats either alone**, mostly by keeping BM25's exact-match wins
   as the store grows; the gap widens with distractors.
4. **Typo repair is the cheapest big win**: correcting query words unknown to
   both the store and the model vocabulary lifts typo recall 0.42 → 0.83 with
   no loss elsewhere. Guarding on the model vocabulary matters — without it
   "cook" was "corrected" to "book".
5. **potion-8M is the sweet spot.** 32M is 4× larger and 2.5× slower for no
   meaningful gain; 2M/4M lose 5–10 points.
6. **int8 quantisation is free**: identical R@5 to f32 for every model, 4×
   smaller.
7. **Static ≈ transformer here.** The shipped hybrid matches bge-small and
   MiniLM dense retrieval on this task, without an ML runtime.
8. **Similarity floor 0.2** halves results returned (10 → 4.9 per query at 78
   memories) for −4 points R@5 small-store, −1 point at 1k. Agents can raise
   `limit`. (The floor is calibrated for potion-8M; retrieval-32M's cosine
   distribution differs, which is why its hybrid row underperforms in
   `results.md`.)
9. **Brute force is enough.** 10k memories: ~1 ms per hybrid search, no ANN index.
10. A **BM25 query-plan bug** (plain JOIN let SQLite scan every memory and probe
    FTS per row) cost 88 ms at 10k; `CROSS JOIN` brought it to 35 µs.

## Footprint (shipped binary, macOS arm64)

| | |
|---|---|
| Binary (incl. model) | 10 MB |
| stdio process, idle, 80 memories | ~13 MB RSS |
| Full session (init + 5 calls) | ~10 ms wall, 14 MB peak RSS |
| Import 78 memories (embed + encrypt) | 20 ms |
| Ingest 10k memories | ~270 ms |

On a Raspberry Pi 5 (Cortex-A76, roughly 3–4× slower single-thread) expect
~0.5 ms searches at 1k memories and the same memory footprint. Not yet
measured on hardware — CI builds for `ubuntu-24.04-arm` (same arch as 64-bit
Pi OS).

## Reproduce

```bash
# models (not committed): minishlab/potion-base-{2M,4M,8M,32M}, potion-retrieval-32M
for m in potion-base-8M; do mkdir -p bench/models/$m
  for f in model.safetensors tokenizer.json config.json; do
    curl -sfL -o bench/models/$m/$f https://huggingface.co/minishlab/$m/resolve/main/$f; done; done
cargo run --release -p recall-bench -- run bench/models/potion-base-8M
cargo run --release -p recall-bench -- floor bench/models/potion-base-8M
uv run --with fastembed python -I bench/reference.py   # transformer ceiling
# rebuild the bundled model
cargo run --release -p recall-bench -- convert bench/models/potion-base-8M models/recall.rv
```
