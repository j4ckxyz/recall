//! Web UI: embedded static assets and a small JSON API behind a password.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::auth;
use crate::store::{Mode, Saved, Store, now};

pub const INDEX_HTML: &str = include_str!("../web/index.html");
pub const APP_JS: &str = include_str!("../web/app.js");
pub const APP_CSS: &str = include_str!("../web/app.css");
pub const ICON_PNG: &[u8] = include_bytes!("../mcpb/icon.png");

pub const PASSWORD_KEY: &str = "web_password";
const COOKIE: &str = "recall_session";
/// Graph size cap; the most-used memories are kept.
const MAX_NODES: usize = 1_500;
const LOGIN_FAILURES_PER_MINUTE: usize = 5;

pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

impl Reply {
    pub fn json(status: u16, v: Value) -> Self {
        Reply { status, content_type: "application/json", body: v.to_string().into_bytes(), headers: Vec::new() }
    }

    fn error(status: u16, msg: &str) -> Self {
        Self::json(status, json!({ "error": msg }))
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub body: &'a [u8],
    pub cookie: Option<&'a str>,
    /// Set by the page's fetch calls; cross-site forms can't add it (CSRF guard).
    pub ajax: bool,
}

/// Failed logins, for rate limiting.
#[derive(Default)]
pub struct Guard {
    failures: VecDeque<Instant>,
}

pub fn api(store: &Store, guard: &mut Guard, req: &Request) -> Reply {
    match route(store, guard, req) {
        Ok(r) => r,
        Err(e) => Reply::error(500, &e.to_string()),
    }
}

fn route(store: &Store, guard: &mut Guard, req: &Request) -> crate::Result<Reply> {
    if req.method != "GET" && !req.ajax {
        return Ok(Reply::error(403, "missing X-Requested-With header"));
    }
    let session = req.cookie.map(auth::session_hash);
    let authed = match &session {
        Some(h) => store.session_valid(h)?,
        None => false,
    };
    let password_set = store.setting(PASSWORD_KEY)?.is_some();

    match (req.method, req.path) {
        ("GET", "/api/session") => return Ok(Reply::json(200, json!({ "authed": authed, "passwordSet": password_set }))),
        ("POST", "/api/login") => return login(store, guard, req),
        ("POST", "/api/logout") => {
            if let Some(h) = &session {
                store.session_remove(Some(h))?;
            }
            let mut r = Reply::json(200, json!({ "ok": true }));
            r.headers.push(("Set-Cookie", format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")));
            return Ok(r);
        }
        _ => {}
    }
    if !authed {
        return Ok(Reply::error(401, "sign in first"));
    }

    Ok(match (req.method, req.path) {
        ("GET", "/api/graph") => {
            let g = store.graph(MAX_NODES)?;
            let nodes: Vec<Value> = g
                .entries
                .iter()
                .zip(&g.cluster)
                .map(|(e, c)| json!({ "id": e.memory.sid, "content": e.memory.content, "date": e.memory.at, "uses": e.uses, "cluster": c }))
                .collect();
            let edges: Vec<Value> = g.edges.iter().map(|(a, b, w)| json!([a, b, (w * 100.0).round() / 100.0])).collect();
            Reply::json(200, json!({ "nodes": nodes, "edges": edges, "clusters": g.labels, "total": store.count()? }))
        }
        ("GET", "/api/search") => {
            let q = param(req.query, "q").unwrap_or_default();
            let hits = store.search(&q, 50, Mode::Hybrid)?;
            Reply::json(200, json!({ "ids": hits.iter().map(|m| &m.sid).collect::<Vec<_>>() }))
        }
        ("POST", "/api/memories") => {
            let body: Value = serde_json::from_slice(req.body).unwrap_or(Value::Null);
            let content = body["content"].as_str().unwrap_or("");
            let date = body["date"].as_str().filter(|d| !d.trim().is_empty());
            let replaces = body["replaces"].as_str().filter(|r| !r.is_empty());
            match store.remember(content, date, replaces, "web") {
                Ok(Saved::New { sid, related }) => {
                    Reply::json(200, json!({ "id": sid, "related": related.iter().map(|m| &m.sid).collect::<Vec<_>>() }))
                }
                Ok(Saved::Replaced { new, .. }) => Reply::json(200, json!({ "id": new })),
                Ok(Saved::Duplicate(m)) => Reply::json(200, json!({ "id": m.sid, "duplicate": true })),
                Err(e) => Reply::error(400, &e.to_string()),
            }
        }
        ("DELETE", p) if p.starts_with("/api/memories/") => {
            let sid = &p["/api/memories/".len()..];
            if store.forget(sid, "web")? { Reply::json(200, json!({ "ok": true })) } else { Reply::error(404, "not found") }
        }
        _ => Reply::error(404, "not found"),
    })
}

fn login(store: &Store, guard: &mut Guard, req: &Request) -> crate::Result<Reply> {
    let t = Instant::now();
    while guard.failures.front().is_some_and(|f| t.duration_since(*f) > Duration::from_secs(60)) {
        guard.failures.pop_front();
    }
    if guard.failures.len() >= LOGIN_FAILURES_PER_MINUTE {
        return Ok(Reply::error(429, "too many attempts; wait a minute"));
    }
    let Some(stored) = store.setting(PASSWORD_KEY)? else {
        return Ok(Reply::error(403, "no password set; run `recall password` on the server"));
    };
    let body: Value = serde_json::from_slice(req.body).unwrap_or(Value::Null);
    if !auth::verify_password(body["password"].as_str().unwrap_or(""), &stored) {
        guard.failures.push_back(t);
        return Ok(Reply::error(401, "wrong password"));
    }
    let (token, hash) = auth::new_session()?;
    store.session_add(&hash, now() + auth::SESSION_SECS)?;
    let mut r = Reply::json(200, json!({ "ok": true }));
    r.headers.push(("Set-Cookie", format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}", auth::SESSION_SECS)));
    Ok(r)
}

/// The session token from a `Cookie` header.
pub fn session_cookie(header: &str) -> Option<&str> {
    header.split(';').filter_map(|c| c.trim().split_once('=')).find(|(k, _)| *k == COOKIE).map(|(_, v)| v)
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push(hex(b[i + 1]).unwrap_or(0) * 16 + hex(b[i + 2]).unwrap_or(0));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(method: &'a str, path: &'a str, body: &'a [u8], cookie: Option<&'a str>) -> Request<'a> {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Request { method, path, query, body, cookie, ajax: true }
    }

    #[test]
    fn login_flow_and_auth() {
        let s = Store::open_in_memory(None).unwrap();
        let mut g = Guard::default();
        assert_eq!(api(&s, &mut g, &req("GET", "/api/graph", b"", None)).status, 401);
        assert_eq!(api(&s, &mut g, &req("POST", "/api/login", br#"{"password":"x"}"#, None)).status, 403);
        s.set_setting(PASSWORD_KEY, Some(&auth::hash_password("hunter22hunter22", 1_000).unwrap())).unwrap();
        assert_eq!(api(&s, &mut g, &req("POST", "/api/login", br#"{"password":"nope"}"#, None)).status, 401);
        let ok = api(&s, &mut g, &req("POST", "/api/login", br#"{"password":"hunter22hunter22"}"#, None));
        assert_eq!(ok.status, 200);
        let cookie = &ok.headers[0].1;
        let token = session_cookie(cookie.split(';').next().unwrap()).unwrap().to_string();

        let r = api(&s, &mut g, &req("POST", "/api/memories", br#"{"content":"User is vegetarian.","date":"2021-03"}"#, Some(&token)));
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let graph: Value = serde_json::from_slice(&api(&s, &mut g, &req("GET", "/api/graph", b"", Some(&token))).body).unwrap();
        assert_eq!(graph["nodes"][0]["content"], "User is vegetarian.");
        let found: Value = serde_json::from_slice(&api(&s, &mut g, &req("GET", "/api/search?q=vegitarian%20food", b"", Some(&token))).body).unwrap();
        assert_eq!(found["ids"].as_array().unwrap().len(), 1);

        let mut csrf = req("POST", "/api/memories", br#"{"content":"x"}"#, Some(&token));
        csrf.ajax = false;
        assert_eq!(api(&s, &mut g, &csrf).status, 403);
    }

    #[test]
    fn rate_limits_logins() {
        let s = Store::open_in_memory(None).unwrap();
        s.set_setting(PASSWORD_KEY, Some(&auth::hash_password("right-password", 1_000).unwrap())).unwrap();
        let mut g = Guard::default();
        for _ in 0..LOGIN_FAILURES_PER_MINUTE {
            assert_eq!(api(&s, &mut g, &req("POST", "/api/login", br#"{"password":"x"}"#, None)).status, 401);
        }
        assert_eq!(api(&s, &mut g, &req("POST", "/api/login", br#"{"password":"right-password"}"#, None)).status, 429);
    }

    #[test]
    fn decodes() {
        assert_eq!(percent_decode("vegitarian%20food+now%"), "vegitarian food now%");
        assert_eq!(session_cookie("a=b; recall_session=abc; c=d"), Some("abc"));
    }
}
