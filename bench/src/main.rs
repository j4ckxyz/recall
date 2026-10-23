//! Retrieval benchmark for Recall.
//!
//! recall-bench run [MODEL_DIR...]   quality + latency table (markdown)
//! recall-bench embed MODEL_DIR      print embeddings of stdin lines (parity checks)
//! recall-bench convert MODEL_DIR OUT  write an int8 bundle
//! recall-bench corpus N            dump facts + N distractors (for reference.py)

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use recall::embed::Embedder;
use recall::store::{Mode, Store};
use serde_json::Value;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data");

struct Query {
    kind: String,
    text: String,
    gold: Vec<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("embed") => {
            let e = Embedder::from_model2vec_dir(Path::new(&args[1])).unwrap();
            for line in std::io::stdin().lines() {
                let v = e.embed(&line.unwrap());
                println!("{}", v.iter().map(|x| format!("{x:.6}")).collect::<Vec<_>>().join(" "));
            }
        }
        Some("convert") => {
            let e = Embedder::from_model2vec_dir(Path::new(&args[1])).unwrap();
            let b = e.to_bundle();
            std::fs::write(&args[2], &b).unwrap();
            eprintln!("wrote {} ({:.1} MB)", args[2], b.len() as f64 / 1e6);
        }
        Some("run") => run(&args[1..]),
        Some("floor") => floor(&args[1]),
        Some("corpus") => {
            // Facts followed by N distractors, one JSON string per line.
            let n = args.get(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            let (facts, _) = load();
            for c in facts.iter().map(|(_, c)| c.clone()).chain(distractors(n)) {
                println!("{}", serde_json::to_string(&c).unwrap());
            }
        }
        _ => eprintln!("usage: recall-bench run [MODEL_DIR...] | embed MODEL_DIR | convert MODEL_DIR OUT | corpus N"),
    }
}

fn load() -> (Vec<(String, String)>, Vec<Query>) {
    let read = |f: &str| -> Vec<Value> {
        std::fs::read_to_string(format!("{DATA}/{f}"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let facts = read("facts.jsonl")
        .into_iter()
        .map(|v| (v["id"].as_str().unwrap().to_string(), v["content"].as_str().unwrap().to_string()))
        .collect();
    let queries = read("queries.jsonl")
        .into_iter()
        .map(|v| Query {
            kind: v["type"].as_str().unwrap().to_string(),
            text: v["query"].as_str().unwrap().to_string(),
            gold: v["gold"].as_array().unwrap().iter().map(|g| g.as_str().unwrap().to_string()).collect(),
        })
        .collect();
    (facts, queries)
}

/// Deterministic, plausible personal-memory noise.
fn distractors(n: usize) -> Vec<String> {
    let people = ["Alex", "Jordan", "Chen", "Fatima", "Lukas", "Mei", "Oscar", "Rhea", "Tomas", "Yuki", "Zara", "Nikolai", "Amara", "Diego", "Freya", "Hugo"];
    let things = ["a standing desk", "a road bike", "noise-cancelling headphones", "a cast-iron pan", "a film camera", "a houseplant", "a mechanical watch", "a tent", "a projector", "a sourdough starter", "a drone", "a kayak"];
    let places = ["Copenhagen", "Seville", "Kyoto", "Vancouver", "Marrakesh", "Reykjavik", "Hanoi", "Krakow", "Lima", "Tallinn", "Cape Town", "Montreal"];
    let topics = ["quantum computing", "urban planning", "medieval history", "bird migration", "behavioural economics", "volcanoes", "typography", "game theory", "coral reefs", "ancient Rome", "climate policy", "linguistics"];
    let verbs = ["bought", "borrowed", "sold", "repaired", "gave away", "was gifted"];
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while out.len() < n {
        let (a, b, c, d) = (i % 16, (i / 16) % 12, (i / 192) % 12, (i / 2304) % 6);
        let year = 2010 + (i % 15);
        out.push(match i % 5 {
            0 => format!("User {} {} in {} {year}.", verbs[d], things[b], places[c]),
            1 => format!("User's acquaintance {} is interested in {} and lives in {}.", people[a], topics[b], places[c]),
            2 => format!("User read an article about {} while visiting {} in {year}.", topics[b], places[c]),
            3 => format!("User once met {} at a {} meetup in {}.", people[a], topics[c], places[b]),
            _ => format!("User has notes on {} from a {year} trip to {}.", topics[(b + c) % 12], places[(a + d) % 12]),
        });
        i += 1;
    }
    out
}

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

/// Sweep the hybrid similarity floor: recall vs. how many results agents get.
fn floor(model_dir: &str) {
    let (facts, queries) = load();
    let emb: &'static Embedder = Box::leak(Box::new(Embedder::from_bundle_owned(Embedder::from_model2vec_dir(Path::new(model_dir)).unwrap().to_bundle()).unwrap()));
    println!("| floor | memories | R@5 | R@10 | MRR | avg results (limit 10) | R@5 task |");
    println!("|---:|---:|---:|---:|---:|---:|---:|");
    for size in [0usize, 1_000] {
        let noise = distractors(size);
        let mut store = Store::open_in_memory(Some(emb)).unwrap();
        let mut items: Vec<(&str, &str)> = facts.iter().map(|(_, c)| (c.as_str(), "2026-01-01")).collect();
        items.extend(noise.iter().map(|c| (c.as_str(), "2020-01-01")));
        let sids = store.insert_many(&items, "bench").unwrap();
        let ids: HashMap<String, String> = sids.iter().zip(&facts).map(|(s, (id, _))| (s.clone(), id.clone())).collect();
        for f in [0.0f32, 0.2, 0.3, 0.35, 0.4, 0.45, 0.5] {
            store.min_similarity = f;
            let returned: usize = queries.iter().map(|q| store.search(&q.text, 10, Mode::Hybrid).unwrap().len()).sum();
            let r = evaluate("", &store, &ids, &queries, Mode::Hybrid, 0);
            let (a, b, c, n) = r.by_type["all"];
            let (t, _, _, tn) = r.by_type["t"];
            println!("| {f:.2} | {} | {:.3} | {:.3} | {:.3} | {:.1} | {:.3} |", facts.len() + size, a / n as f64, b / n as f64, c / n as f64, returned as f64 / queries.len() as f64, t / tn as f64);
        }
    }
}

struct Row {
    name: String,
    size: usize,
    by_type: HashMap<String, (f64, f64, f64, usize)>, // R@5, R@10, MRR, n
    p50_us: f64,
    p99_us: f64,
}

fn evaluate(name: &str, store: &Store, ids: &HashMap<String, String>, queries: &[Query], mode: Mode, size: usize) -> Row {
    let mut by_type: HashMap<String, (f64, f64, f64, usize)> = HashMap::new();
    let mut lat = Vec::new();
    for q in queries {
        let t = Instant::now();
        let hits = store.search(&q.text, 10, mode).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
        let ranked: Vec<&str> = hits.iter().map(|m| ids.get(&m.sid).map(String::as_str).unwrap_or("")).collect();
        let found = |k: usize| q.gold.iter().filter(|g| ranked.iter().take(k).any(|r| r == g)).count() as f64 / q.gold.len() as f64;
        let mrr = ranked.iter().position(|r| q.gold.iter().any(|g| g == r)).map_or(0.0, |p| 1.0 / (p as f64 + 1.0));
        for key in [q.kind.as_str(), "all"] {
            let e = by_type.entry(key.to_string()).or_default();
            e.0 += found(5);
            e.1 += found(10);
            e.2 += mrr;
            e.3 += 1;
        }
    }
    lat.sort_by(f64::total_cmp);
    Row {
        name: name.to_string(),
        size,
        by_type,
        p50_us: lat[lat.len() / 2],
        p99_us: lat[(lat.len() * 99 / 100).min(lat.len() - 1)],
    }
}

fn run(model_dirs: &[String]) {
    let (facts, queries) = load();
    let mut models: Vec<(String, Option<&'static Embedder>)> = vec![("none".into(), None)];
    for d in model_dirs {
        let name = Path::new(d).file_name().unwrap().to_string_lossy().to_string();
        let before = rss_mb();
        let t = Instant::now();
        let f32m: &'static Embedder = Box::leak(Box::new(Embedder::from_model2vec_dir(Path::new(d)).unwrap()));
        let bundle: &'static [u8] = Box::leak(f32m.to_bundle().into_boxed_slice());
        let int8: &'static Embedder = Box::leak(Box::new(Embedder::from_bundle(bundle).unwrap()));
        eprintln!("{name}: loaded in {:?}, bundle {:.1} MB, +{:.0} MB RSS (f32 + int8 copies)", t.elapsed(), bundle.len() as f64 / 1e6, rss_mb() - before);
        models.push((format!("{name}/f32"), Some(f32m)));
        models.push((format!("{name}/int8"), Some(int8)));
    }

    let mut rows = Vec::new();
    for size in [0usize, 1_000, 10_000] {
        let noise = distractors(size);
        for (mname, emb) in &models {
            let store = Store::open_in_memory(*emb).unwrap();
            let mut items: Vec<(&str, &str)> = facts.iter().map(|(_, c)| (c.as_str(), "2026-01-01")).collect();
            items.extend(noise.iter().map(|c| (c.as_str(), "2020-01-01")));
            let t = Instant::now();
            let sids = store.insert_many(&items, "bench").unwrap();
            let ingest = t.elapsed();
            let ids: HashMap<String, String> = sids.iter().zip(&facts).map(|(s, (id, _))| (s.clone(), id.clone())).collect();
            let modes: &[(&str, Mode)] = if emb.is_none() {
                &[("bm25-all", Mode::Bm25All), ("bm25-any", Mode::Bm25Any)]
            } else {
                &[("dense", Mode::Dense), ("hybrid-raw", Mode::HybridRaw), ("hybrid", Mode::Hybrid)]
            };
            for (label, mode) in modes {
                // Warm the vector cache so p50 reflects steady state.
                store.search("warm up", 1, *mode).unwrap();
                let name = if emb.is_none() { label.to_string() } else { format!("{label} {mname}") };
                rows.push(evaluate(&name, &store, &ids, &queries, *mode, facts.len() + size));
            }
            eprintln!("size {} {mname}: ingest {:?}", facts.len() + size, ingest);
        }
    }

    let types = ["all", "k", "p", "t", "y"];
    println!("| strategy | memories | R@5 all | R@10 all | MRR all | R@5 keyword | R@5 paraphrase | R@5 task | R@5 typo | p50 µs | p99 µs |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for r in &rows {
        let get = |t: &str, i: usize| {
            let (a, b, c, n) = r.by_type[t];
            [a, b, c][i] / n as f64
        };
        let mut line = format!("| {} | {} | {:.3} | {:.3} | {:.3}", r.name, r.size, get("all", 0), get("all", 1), get("all", 2));
        for t in &types[1..] {
            line.push_str(&format!(" | {:.3}", get(t, 0)));
        }
        line.push_str(&format!(" | {:.0} | {:.0} |", r.p50_us, r.p99_us));
        println!("{line}");
    }
}
