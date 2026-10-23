//! Encrypted SQLite store: memories, full-text index, vectors, audit log, HTTP tokens.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::config::{Key, hex};
use crate::embed::{self, Embedder};
use crate::text::{fts_query, word_set, words};
use crate::{Result, guard, when};

/// Active memories cap; protects against a runaway agent.
pub const MAX_ACTIVE: i64 = 5_000;

/// Soft-deleted memories are kept this long before `purge` removes them.
pub const RETENTION_SECS: i64 = 30 * 24 * 3600;

/// Default `Store::min_similarity`, chosen by `recall-bench`.
pub const MIN_SIMILARITY: f32 = 0.2;

/// Cosine similarity at or above which an existing memory is reported as
/// related when saving, so the agent can spot a contradiction.
pub const RELATED_COS: f32 = 0.6;

#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub sid: String,
    pub content: String,
    /// When the fact was learned or became true (see `when`).
    pub at: String,
}

#[derive(Debug, PartialEq)]
pub enum Saved {
    New { sid: String, related: Vec<Memory> },
    Replaced { old: String, new: String },
    Duplicate(Memory),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    Read,
    Write,
}

/// Retrieval strategy. `Hybrid` is what agents get; the rest exist for `bench`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// BM25, every term must match.
    Bm25All,
    /// BM25, any term may match.
    Bm25Any,
    /// Cosine similarity of static embeddings.
    Dense,
    /// Reciprocal-rank fusion of `Bm25Any` and `Dense`.
    HybridRaw,
    /// `HybridRaw` after correcting query typos against the store vocabulary.
    Hybrid,
}

pub struct Store {
    conn: Connection,
    embedder: Option<&'static Embedder>,
    /// Dense hits below this cosine are dropped unless BM25 also found them.
    pub min_similarity: f32,
    cache: RefCell<Option<Cache>>,
}

/// Derived from active memories; rebuilt when `PRAGMA data_version` changes.
struct Cache {
    version: i64,
    vectors: Vec<(i64, Vec<f32>)>,
    /// Content word -> number of memories containing it.
    vocab: HashMap<String, u32>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories(
  id INTEGER PRIMARY KEY,
  sid TEXT NOT NULL UNIQUE,
  content TEXT NOT NULL,
  at TEXT NOT NULL,
  vec BLOB,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER,
  use_count INTEGER NOT NULL DEFAULT 0,
  deleted_at INTEGER
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  content, content='memories', content_rowid='id',
  tokenize='porter unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;
CREATE TABLE IF NOT EXISTS audit(
  ts INTEGER NOT NULL, op TEXT NOT NULL, sid TEXT, client TEXT
);
CREATE TABLE IF NOT EXISTS tokens(
  name TEXT PRIMARY KEY, hash BLOB NOT NULL UNIQUE, scope TEXT NOT NULL, created_at INTEGER NOT NULL
);
PRAGMA user_version = 1;
";

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

const COLS: &str = "sid, content, at";

impl Store {
    /// Open (creating if needed) the encrypted store at `path`.
    pub fn open(path: &Path, key: &Key, embedder: Option<&'static Embedder>) -> Result<Self> {
        if !path.exists() {
            // Pre-create with 0600; SQLite copies these permissions to -wal/-shm.
            OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Raw-key form skips PBKDF2, so opening costs microseconds, not ~100 ms.
        conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", key.hex()))?;
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .map_err(|_| "cannot decrypt store: wrong key or not a Recall database")?;
        Self::init(conn, embedder, true)
    }

    /// Unencrypted in-memory store for tests and benchmarks.
    pub fn open_in_memory(embedder: Option<&'static Embedder>) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, embedder, false)
    }

    fn init(conn: Connection, embedder: Option<&'static Embedder>, wal: bool) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000;
             PRAGMA secure_delete = ON;
             PRAGMA cache_size = -1024;
             PRAGMA temp_store = MEMORY;",
        )?;
        if wal {
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        }
        conn.execute_batch(SCHEMA)?;
        let s = Store { conn, embedder, min_similarity: MIN_SIMILARITY, cache: RefCell::new(None) };
        s.backfill_vectors()?;
        Ok(s)
    }

    /// Embed any memories saved without a vector (e.g. before a model was added).
    fn backfill_vectors(&self) -> Result<()> {
        let Some(e) = self.embedder else { return Ok(()) };
        let dim_bytes = (e.dim() * 4) as i64;
        let missing: Vec<(i64, String)> = self
            .conn
            .prepare("SELECT id, content FROM memories WHERE vec IS NULL OR length(vec) != ?1")?
            .query_map([dim_bytes], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = self.conn.prepare("UPDATE memories SET vec = ?1 WHERE id = ?2")?;
        for (id, content) in missing {
            stmt.execute(params![embed::to_bytes(&e.embed(&content)), id])?;
        }
        Ok(())
    }

    // ---- reads -------------------------------------------------------------

    pub fn get(&self, sid: &str) -> Result<Option<Memory>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {COLS} FROM memories WHERE sid = ?1 AND deleted_at IS NULL"), [sid], row_to_memory)
            .optional()?)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM memories WHERE deleted_at IS NULL", [], |r| r.get(0))?)
    }

    /// Every active memory, most-used and most-recent first.
    pub fn all(&self) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLS} FROM memories WHERE deleted_at IS NULL ORDER BY use_count DESC, at DESC, id DESC"
        ))?;
        let rows = stmt.query_map([], row_to_memory)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Memories that fit within `budget_chars`, plus how many were left out.
    pub fn profile(&self, budget_chars: usize) -> Result<(Vec<Memory>, usize)> {
        let all = self.all()?;
        let mut used = 0;
        let mut out = Vec::new();
        for m in &all {
            let cost = m.content.len() + m.at.len() + 10;
            if used + cost <= budget_chars {
                used += cost;
                out.push(m.clone());
            }
        }
        let skipped = all.len() - out.len();
        Ok((out, skipped))
    }

    /// Ranked search. Falls back to BM25 when no embedding model is loaded.
    pub fn search(&self, query: &str, limit: usize, mode: Mode) -> Result<Vec<Memory>> {
        let ids: Vec<i64> = match (mode, self.embedder) {
            (Mode::Bm25All, _) => self.bm25(query, limit, false)?,
            (Mode::Bm25Any, _) => self.bm25(query, limit, true)?,
            (_, None) => self.bm25(&self.fix_typos(query)?, limit, true)?,
            (Mode::Dense, Some(e)) => self.dense(&e.embed(query), limit)?.into_iter().map(|(id, _)| id).collect(),
            (Mode::HybridRaw | Mode::Hybrid, Some(e)) => {
                let q = if mode == Mode::Hybrid { self.fix_typos(query)? } else { query.to_string() };
                let pool = limit.max(30);
                let lexical = self.bm25(&q, pool, true)?;
                let semantic: Vec<i64> = self
                    .dense(&e.embed(&q), pool)?
                    .into_iter()
                    .filter(|(id, cos)| *cos >= self.min_similarity || lexical.contains(id))
                    .map(|(id, _)| id)
                    .collect();
                rrf(&[&lexical, &semantic], limit)
            }
        };
        self.by_ids(&ids)
    }

    fn bm25(&self, query: &str, limit: usize, any: bool) -> Result<Vec<i64>> {
        let Some(q) = fts_query(query, any) else { return Ok(Vec::new()) };
        // CROSS JOIN pins the FTS index as the outer loop; a plain JOIN lets the
        // planner scan every memory and probe the index per row (100x slower).
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.id FROM memories_fts CROSS JOIN memories m ON m.id = memories_fts.rowid
             WHERE memories_fts MATCH ?1 AND m.deleted_at IS NULL
             ORDER BY memories_fts.rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![q, limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Append spelling corrections for query words that are neither in the
    /// store nor real words to the model ("vegitarian" gains "vegetarian";
    /// "cook" is left alone even if the store only knows "book").
    fn fix_typos(&self, query: &str) -> Result<String> {
        self.refresh_cache()?;
        let cache = self.cache.borrow();
        let vocab = &cache.as_ref().expect("refreshed").vocab;
        let mut out = query.to_string();
        for w in words(query) {
            let n = w.chars().count();
            let real_word = self.embedder.is_some_and(|e| e.knows(&w));
            if n < 4 || real_word || vocab.contains_key(&w) || w.chars().any(|c| c.is_ascii_digit()) {
                continue;
            }
            let max = if n >= 8 { 2 } else { 1 };
            let best = vocab
                .iter()
                .filter(|(v, _)| v.chars().count().abs_diff(n) <= max)
                .filter_map(|(v, df)| edit_distance(&w, v, max).map(|d| (d, *df, v)))
                .min_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
            if let Some((_, _, v)) = best {
                out.push(' ');
                out.push_str(v);
            }
        }
        Ok(out)
    }

    /// Brute-force cosine over all active vectors. At personal scale (thousands
    /// of memories) this is sub-millisecond and needs no index.
    fn dense(&self, q: &[f32], limit: usize) -> Result<Vec<(i64, f32)>> {
        if q.iter().all(|x| *x == 0.0) {
            return Ok(Vec::new());
        }
        self.refresh_cache()?;
        let cache = self.cache.borrow();
        let rows = &cache.as_ref().expect("refreshed").vectors;
        let mut scored: Vec<(i64, f32)> = rows.iter().map(|(id, v)| (*id, embed::dot(q, v))).collect();
        let k = limit.min(scored.len());
        if k == 0 {
            return Ok(Vec::new());
        }
        scored.select_nth_unstable_by(k - 1, |a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        Ok(scored)
    }

    /// Reload the cache if any connection has written since last time.
    fn refresh_cache(&self) -> Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        if matches!(&*self.cache.borrow(), Some(c) if c.version == version) {
            return Ok(());
        }
        let mut vectors = Vec::new();
        let mut vocab: HashMap<String, u32> = HashMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT id, content, vec FROM memories WHERE deleted_at IS NULL")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            for w in words(&r.get::<_, String>(1)?) {
                *vocab.entry(w).or_default() += 1;
            }
            if let Some(v) = r.get::<_, Option<Vec<u8>>>(2)? {
                vectors.push((r.get(0)?, embed::from_bytes(&v)));
            }
        }
        *self.cache.borrow_mut() = Some(Cache { version, vectors, vocab });
        Ok(())
    }

    /// Our own writes don't bump `data_version` for this connection.
    fn invalidate(&self) {
        *self.cache.borrow_mut() = None;
    }

    fn by_ids(&self, ids: &[i64]) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {COLS} FROM memories WHERE id = ?1"))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(m) = stmt.query_row([id], row_to_memory).optional()? {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// Record that memories were shown to an agent (drives profile ordering).
    pub fn touch(&self, sids: &[&str]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE memories SET use_count = use_count + 1, last_used_at = ?1 WHERE sid = ?2")?;
        let t = now();
        for s in sids {
            stmt.execute(params![t, s])?;
        }
        Ok(())
    }

    // ---- writes ------------------------------------------------------------

    /// Save a fact. `at` is when it was learned (defaults to now); `replaces`
    /// supersedes an existing memory. Duplicates are absorbed server-side.
    pub fn remember(&self, content: &str, at: Option<&str>, replaces: Option<&str>, client: &str) -> Result<Saved> {
        let content = guard::normalise(content);
        let n = content.chars().count();
        if n == 0 {
            return Err("content is empty".into());
        }
        if n > guard::MAX_CHARS {
            return Err(format!("content is {n} characters; the limit is {}. Save one concise fact.", guard::MAX_CHARS).into());
        }
        if let Some(why) = guard::secret_reason(&content) {
            return Err(format!("not saved: this looks like {why}. Secrets and identifiers must not be stored in memory.").into());
        }
        let at = match at {
            Some(a) => when::parse(a)?,
            None => when::now(),
        };
        let old = match replaces {
            Some(sid) => Some(self.get(sid.trim_matches(['[', ']']))?.ok_or_else(|| format!("no memory with id [{sid}]"))?),
            None => None,
        };
        let vec = self.embedder.map(|e| e.embed(&content));

        let mut related = Vec::new();
        if old.is_none() {
            let key = word_set(&content);
            let candidates = match &vec {
                Some(v) => self.dense(v, 3)?.into_iter().filter(|(_, s)| *s >= RELATED_COS).map(|(id, _)| id).collect(),
                None => self.bm25(&content, 3, true)?,
            };
            for m in self.by_ids(&candidates)? {
                if word_set(&m.content) == key {
                    return Ok(Saved::Duplicate(m));
                }
                related.push(m);
            }
            if self.count()? >= MAX_ACTIVE {
                return Err(format!("memory is full ({MAX_ACTIVE} facts); forget outdated ones first").into());
            }
        }

        let tx = self.conn.unchecked_transaction()?;
        let sid = self.fresh_sid()?;
        tx.execute(
            "INSERT INTO memories(sid, content, at, vec, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![sid, content, at, vec.as_deref().map(embed::to_bytes), now()],
        )?;
        self.audit("add", &sid, client)?;
        let saved = if let Some(old) = old {
            // Replacement is a soft delete + insert so the owner can undo it.
            tx.execute("UPDATE memories SET deleted_at = ?1 WHERE sid = ?2", params![now(), old.sid])?;
            self.audit("replace", &old.sid, client)?;
            Saved::Replaced { old: old.sid, new: sid }
        } else {
            Saved::New { sid, related }
        };
        tx.commit()?;
        self.invalidate();
        Ok(saved)
    }

    /// Bulk insert of trusted owner data (benchmarks, large imports): applies
    /// the guards but skips duplicate detection. Returns the new ids.
    pub fn insert_many(&self, items: &[(&str, &str)], client: &str) -> Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut sids = Vec::with_capacity(items.len());
        for (content, at) in items {
            let content = guard::normalise(content);
            if content.is_empty() || content.chars().count() > guard::MAX_CHARS || guard::secret_reason(&content).is_some() {
                return Err(format!("rejected: {content}").into());
            }
            let at = when::parse(at)?;
            let sid = self.fresh_sid()?;
            let vec = self.embedder.map(|e| embed::to_bytes(&e.embed(&content)));
            tx.execute(
                "INSERT INTO memories(sid, content, at, vec, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![sid, content, at, vec, now()],
            )?;
            self.audit("add", &sid, client)?;
            sids.push(sid);
        }
        tx.commit()?;
        self.invalidate();
        Ok(sids)
    }

    /// Soft delete; restorable for `RETENTION_SECS`.
    pub fn forget(&self, sid: &str, client: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("UPDATE memories SET deleted_at = ?1 WHERE sid = ?2 AND deleted_at IS NULL", params![now(), sid])?;
        if n > 0 {
            self.audit("forget", sid, client)?;
            self.invalidate();
        }
        Ok(n > 0)
    }

    pub fn restore(&self, sid: &str) -> Result<bool> {
        let n = self.conn.execute("UPDATE memories SET deleted_at = NULL WHERE sid = ?1 AND deleted_at IS NOT NULL", [sid])?;
        if n > 0 {
            self.audit("restore", sid, "owner")?;
            self.invalidate();
        }
        Ok(n > 0)
    }

    /// Hard-delete soft-deleted rows older than `older_than` seconds.
    pub fn purge(&self, older_than: i64) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM memories WHERE deleted_at IS NOT NULL AND deleted_at <= ?1",
            [now() - older_than],
        )?;
        self.conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES ('optimize'); VACUUM;")?;
        Ok(n)
    }

    /// Soft-deleted memories, newest first.
    pub fn trash(&self) -> Result<Vec<Memory>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLS} FROM memories WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC"))?;
        let rows = stmt.query_map([], row_to_memory)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn audit(&self, op: &str, sid: &str, client: &str) -> Result<()> {
        self.conn
            .prepare_cached("INSERT INTO audit(ts, op, sid, client) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![now(), op, sid, client])?;
        Ok(())
    }

    pub fn audit_log(&self, limit: usize) -> Result<Vec<(i64, String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, op, coalesce(sid, ''), coalesce(client, '') FROM audit ORDER BY rowid DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn fresh_sid(&self) -> Result<String> {
        const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
        loop {
            let mut raw = [0u8; 5];
            getrandom::fill(&mut raw).map_err(|e| format!("rng: {e}"))?;
            let sid: String = raw.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
            let taken: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM memories WHERE sid = ?1)", [&sid], |r| r.get(0))?;
            if !taken {
                return Ok(sid);
            }
        }
    }

    // ---- HTTP tokens -------------------------------------------------------

    /// Create a token; returns the plaintext once. Only its SHA-256 is stored.
    pub fn token_add(&self, name: &str, scope: Scope) -> Result<String> {
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| format!("rng: {e}"))?;
        let token = format!("rcl_{}", hex(&raw));
        let scope = if scope == Scope::Write { "write" } else { "read" };
        self.conn
            .execute(
                "INSERT INTO tokens(name, hash, scope, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![name, Sha256::digest(token.as_bytes()).as_slice(), scope, now()],
            )
            .map_err(|_| format!("token '{name}' already exists"))?;
        Ok(token)
    }

    pub fn token_revoke(&self, name: &str) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM tokens WHERE name = ?1", [name])? > 0)
    }

    pub fn token_list(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT name, scope FROM tokens ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Resolve a bearer token to (name, scope). Looks up by hash, so the
    /// comparison never touches the secret byte-by-byte.
    pub fn token_check(&self, token: &str) -> Result<Option<(String, Scope)>> {
        let hash = Sha256::digest(token.as_bytes());
        Ok(self
            .conn
            .prepare_cached("SELECT name, scope FROM tokens WHERE hash = ?1")?
            .query_row([hash.as_slice()], |r| {
                let scope: String = r.get(1)?;
                Ok((r.get(0)?, if scope == "write" { Scope::Write } else { Scope::Read }))
            })
            .optional()?)
    }
}

/// Optimal-string-alignment distance, or `None` if it exceeds `max`.
fn edit_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        let mut best = usize::MAX;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (rows[i - 1][j] + 1).min(rows[i][j - 1] + 1).min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = d;
            best = best.min(d);
        }
        if best > max {
            return None;
        }
    }
    Some(rows[a.len()][b.len()]).filter(|d| *d <= max)
}

/// Reciprocal-rank fusion (k = 60, Cormack et al. 2009).
fn rrf(lists: &[&[i64]], limit: usize) -> Vec<i64> {
    let mut score: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *score.entry(*id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
        }
    }
    let mut ranked: Vec<(i64, f64)> = score.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.into_iter().take(limit).map(|(id, _)| id).collect()
}

fn row_to_memory(r: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory { sid: r.get(0)?, content: r.get(1)?, at: r.get(2)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory(None).unwrap()
    }

    fn sid(s: Saved) -> String {
        match s {
            Saved::New { sid, .. } => sid,
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn save_search_forget_restore() {
        let s = store();
        let a = sid(s.remember("User is vegetarian.", Some("2024-05-01"), None, "t").unwrap());
        sid(s.remember("User lives in Lisbon, Portugal.", None, None, "t").unwrap());
        let hits = s.search("what food does the user eat? vegetarian?", 5, Mode::Hybrid).unwrap();
        assert_eq!(hits[0].sid, a);
        assert_eq!(hits[0].at, "2024-05-01");
        assert!(s.forget(&a, "t").unwrap());
        assert!(s.search("vegetarian", 5, Mode::Bm25Any).unwrap().is_empty());
        assert!(s.restore(&a).unwrap());
        assert_eq!(s.search("vegetarian", 5, Mode::Bm25Any).unwrap().len(), 1);
    }

    #[test]
    fn duplicates_absorbed_related_reported() {
        let s = store();
        let a = sid(s.remember("User prefers dark mode in every editor.", None, None, "t").unwrap());
        assert!(matches!(s.remember("user prefers dark mode, in every editor", None, None, "t").unwrap(), Saved::Duplicate(m) if m.sid == a));
        let Saved::New { related, .. } = s.remember("User prefers light mode in every editor.", None, None, "t").unwrap() else { panic!() };
        assert_eq!(related[0].sid, a);
    }

    #[test]
    fn replace_is_undoable() {
        let s = store();
        let a = sid(s.remember("User lives in London.", None, None, "t").unwrap());
        let Saved::Replaced { old, new } = s.remember("User lives in Berlin.", Some("2026-01"), Some(&a), "t").unwrap() else { panic!() };
        assert_eq!(old, a);
        assert!(s.get(&a).unwrap().is_none());
        assert_eq!(s.get(&new).unwrap().unwrap().at, "2026-01");
        assert!(s.restore(&a).unwrap());
    }

    #[test]
    fn rejects_bad_input() {
        let s = store();
        assert!(s.remember("my github token is ghp_abcdefghijklmnopqrstuvwxyz0123456789", None, None, "t").is_err());
        assert!(s.remember(&"x".repeat(501), None, None, "t").is_err());
        assert!(s.remember("fine", Some("last week"), None, "t").is_err());
    }

    #[test]
    fn tokens() {
        let s = store();
        let t = s.token_add("phone", Scope::Read).unwrap();
        assert_eq!(s.token_check(&t).unwrap(), Some(("phone".into(), Scope::Read)));
        assert_eq!(s.token_check("rcl_nope").unwrap(), None);
        assert!(s.token_revoke("phone").unwrap());
        assert_eq!(s.token_check(&t).unwrap(), None);
    }

    #[test]
    fn profile_budgets() {
        let s = store();
        s.remember("User likes hiking.", None, None, "t").unwrap();
        s.remember("Always answer in British English.", None, None, "t").unwrap();
        let (_, skipped) = s.profile(10_000).unwrap();
        assert_eq!(skipped, 0);
        let (p, skipped) = s.profile(50).unwrap();
        assert_eq!((p.len(), skipped), (1, 1));
    }

    #[test]
    fn fixes_typos() {
        let s = store();
        s.remember("User is vegetarian.", None, None, "t").unwrap();
        s.remember("User has a rescue greyhound.", None, None, "t").unwrap();
        assert_eq!(s.search("vegitarian", 5, Mode::Hybrid).unwrap()[0].content, "User is vegetarian.");
        assert_eq!(s.search("greyhund", 5, Mode::Hybrid).unwrap()[0].content, "User has a rescue greyhound.");
        assert_eq!(edit_distance("freind", "friend", 1), Some(1));
        assert_eq!(edit_distance("cat", "dog", 2), None);
    }

    #[test]
    fn rrf_fuses() {
        assert_eq!(rrf(&[&[1, 2, 3], &[3, 1, 4]], 3), vec![1, 3, 2]);
    }
}
