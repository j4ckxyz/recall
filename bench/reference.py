"""Quality ceiling: dense retrieval with transformer embedders (not shipped).

uv run --with fastembed python -I bench/reference.py
"""
import json, subprocess, sys, pathlib
import numpy as np
from fastembed import TextEmbedding

root = pathlib.Path(__file__).resolve().parent
facts = [json.loads(l) for l in open(root / "data/facts.jsonl")]
queries = [json.loads(l) for l in open(root / "data/queries.jsonl")]
ids = [f["id"] for f in facts]

for model in ["BAAI/bge-small-en-v1.5", "sentence-transformers/all-MiniLM-L6-v2"]:
    m = TextEmbedding(model)
    qv = np.array(list(m.embed([q["query"] for q in queries])))
    for n in [0, 1000]:
        corpus = [json.loads(l) for l in subprocess.run(
            ["cargo", "run", "-q", "--release", "-p", "recall-bench", "--", "corpus", str(n)],
            capture_output=True, text=True, check=True, cwd=root.parent).stdout.splitlines()]
        dv = np.array(list(m.embed(corpus)))
        dv /= np.linalg.norm(dv, axis=1, keepdims=True)
        q = qv / np.linalg.norm(qv, axis=1, keepdims=True)
        top = np.argsort(-(q @ dv.T), axis=1)[:, :5]
        by = {}
        for qi, query in enumerate(queries):
            got = {ids[i] for i in top[qi] if i < len(ids)}
            r = len(got & set(query["gold"])) / len(query["gold"])
            for k in (query["type"], "all"):
                by.setdefault(k, []).append(r)
        print(f"| dense {model.split('/')[1]} (reference) | {len(corpus)} | "
              + f"{np.mean(by['all']):.3f} | – | – | " + " | ".join(f"{np.mean(by[k]):.3f}" for k in ["k", "p", "t", "y"]) + " | – | – |")
