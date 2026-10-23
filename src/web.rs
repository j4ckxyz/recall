//! Web UI: embedded static assets and a small JSON API behind a password,
//! or automatic sign-in by Tailscale identity on a tailnet.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::auth;
use crate::store::{Mode, Saved, Store, now};
use crate::tailnet::Identity;

pub const INDEX_HTML: &str = include_str!("../web/index.html");
pub const APP_JS: &str = include_str!("../web/app.js");
pub const APP_CSS: &str = include_str!("../web/app.css");
pub const ICON_PNG: &[u8] = include_bytes!("../mcpb/icon.png");

pub const PASSWORD_KEY: &str = "web_password";
/// "off" disables Tailscale sign-in.
pub const TAILNET_KEY: &str = "tailnet_login";
/// Extra Tailscale logins allowed besides the server's owner, one per line.
pub const TAILNET_ALLOW_KEY: &str = "tailnet_allow";
const COOKIE: &str = "recall_session";
/// Set on sign-out so Tailscale sign-in doesn't immediately sign you back in.
const SIGNED_OUT: &str = "recall_signed_out";
/// Tailscale sessions are short; they renew silently while you're on the tailnet.
const TAILNET_SESSION_SECS: i64 = 24 * 3600;
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
    /// The tailnet user making the request, when known (only resolved for
    /// sign-in requests).
    pub tailnet: Option<Identity>,
    /// Login of the user who owns this server's tailnet device.
    pub owner: Option<&'a str>,
    /// The browser signed out this session; don't sign it back in by itself.
    pub signed_out: bool,
}

/// May this tailnet user use the web UI?
pub fn tailnet_allowed(store: &Store, owner: Option<&str>, id: &Identity) -> crate::Result<bool> {
    if id.tagged || store.setting(TAILNET_KEY)?.as_deref() == Some("off") {
        return Ok(false);
    }
    let extra = store.setting(TAILNET_ALLOW_KEY)?.unwrap_or_default();
    Ok(owner == Some(id.login.as_str()) || extra.lines().any(|l| l.trim().eq_ignore_ascii_case(&id.login)))
}

fn session_cookie_header(token: &str, max_age: i64) -> String {
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}")
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
    let user = match &session {
        Some(h) => store.session_user(h)?,
        None => None,
    };
    let password_set = store.setting(PASSWORD_KEY)?.is_some();

    match (req.method, req.path) {
        ("GET", "/api/session") => {
            let tailnet = match &req.tailnet {
                Some(id) => Some((id, tailnet_allowed(store, req.owner, id)?)),
                None => None,
            };
            let mut reply_headers = Vec::new();
            let mut user = user;
            // Instant sign-in: an allowed tailnet user gets a session without a
            // prompt (page fetches only, so a cross-site image can't trigger it).
            if user.is_none()
                && req.ajax
                && !req.signed_out
                && let Some((id, true)) = tailnet
            {
                reply_headers.push(("Set-Cookie", tailnet_session(store, id)?));
                user = Some(id.login.clone());
            }
            let mut r = Reply::json(200, json!({
                "authed": user.is_some(),
                "user": user,
                "passwordSet": password_set,
                "tailnet": tailnet.map(|(id, allowed)| json!({ "login": id.login, "name": id.name, "allowed": allowed })),
            }));
            r.headers = reply_headers;
            return Ok(r);
        }
        ("POST", "/api/login") => {
            let body: Value = serde_json::from_slice(req.body).unwrap_or(Value::Null);
            if body["tailscale"] == true {
                let Some(id) = &req.tailnet else { return Ok(Reply::error(403, "not connected over your tailnet")) };
                if !tailnet_allowed(store, req.owner, id)? {
                    return Ok(Reply::error(403, &format!("{} isn't allowed; on the server run: recall tailnet allow {}", id.login, id.login)));
                }
                let mut r = Reply::json(200, json!({ "ok": true }));
                r.headers.push(("Set-Cookie", tailnet_session(store, id)?));
                r.headers.push(("Set-Cookie", format!("{SIGNED_OUT}=; Path=/; SameSite=Strict; Max-Age=0")));
                return Ok(r);
            }
            return login(store, guard, req);
        }
        ("POST", "/api/logout") => {
            if let Some(h) = &session {
                store.session_remove(Some(h))?;
            }
            let mut r = Reply::json(200, json!({ "ok": true }));
            r.headers.push(("Set-Cookie", session_cookie_header("", 0)));
            r.headers.push(("Set-Cookie", format!("{SIGNED_OUT}=1; Path=/; SameSite=Strict")));
            return Ok(r);
        }
        _ => {}
    }
    let authed = user.is_some();
    let client = match user.as_deref() {
        Some("password") | None => "web".to_string(),
        Some(login) => format!("web ({login})"),
    };
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
            match store.remember(content, date, replaces, &client) {
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
            if store.forget(sid, &client)? { Reply::json(200, json!({ "ok": true })) } else { Reply::error(404, "not found") }
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
    store.session_add(&hash, now() + auth::SESSION_SECS, None)?;
    let mut r = Reply::json(200, json!({ "ok": true }));
    r.headers.push(("Set-Cookie", session_cookie_header(&token, auth::SESSION_SECS)));
    r.headers.push(("Set-Cookie", format!("{SIGNED_OUT}=; Path=/; SameSite=Strict; Max-Age=0")));
    Ok(r)
}

/// Start a session for a tailnet user; returns its Set-Cookie value.
fn tailnet_session(store: &Store, id: &Identity) -> crate::Result<String> {
    let (token, hash) = auth::new_session()?;
    store.session_add(&hash, now() + TAILNET_SESSION_SECS, Some(&id.login))?;
    Ok(session_cookie_header(&token, TAILNET_SESSION_SECS))
}

/// The session token from a `Cookie` header.
pub fn session_cookie(header: &str) -> Option<&str> {
    cookie(header, COOKIE)
}

/// Whether the browser signed out this session (see `SIGNED_OUT`).
pub fn signed_out(header: &str) -> bool {
    cookie(header, SIGNED_OUT) == Some("1")
}

fn cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').filter_map(|c| c.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v)
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
        Request { method, path, query, body, cookie, ajax: true, tailnet: None, owner: None, signed_out: false }
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
    fn tailnet_sign_in() {
        let s = Store::open_in_memory(None).unwrap();
        let mut g = Guard::default();
        let jack = Identity { login: "jack@example.com".into(), name: "Jack".into(), tagged: false };
        let eve = Identity { login: "eve@example.com".into(), name: "Eve".into(), tagged: false };
        let ci = Identity { login: "tagged-devices".into(), name: "ci".into(), tagged: true };
        let session = |id: &Identity, signed_out: bool, ajax: bool| {
            let mut r = req("GET", "/api/session", b"", None);
            r.tailnet = Some(id.clone());
            r.owner = Some("jack@example.com");
            r.signed_out = signed_out;
            r.ajax = ajax;
            r
        };
        let body = |r: &Reply| serde_json::from_slice::<Value>(&r.body).unwrap();

        // The owner is signed in instantly and can use the API.
        let r = api(&s, &mut g, &session(&jack, false, true));
        assert_eq!(body(&r)["authed"], true);
        let token = session_cookie(r.headers[0].1.split(';').next().unwrap()).unwrap().to_string();
        assert_eq!(api(&s, &mut g, &req("GET", "/api/graph", b"", Some(&token))).status, 200);
        // Writes are attributed to the Tailscale user in the audit log.
        api(&s, &mut g, &req("POST", "/api/memories", br#"{"content":"User likes owls."}"#, Some(&token)));
        assert_eq!(s.audit_log(1).unwrap()[0].3, "web (jack@example.com)");

        // Not after signing out, nor from a non-page request, other users or tagged devices.
        assert_eq!(body(&api(&s, &mut g, &session(&jack, true, true)))["authed"], false);
        assert_eq!(body(&api(&s, &mut g, &session(&jack, false, false)))["authed"], false);
        let r = body(&api(&s, &mut g, &session(&eve, false, true)));
        assert_eq!((r["authed"].clone(), r["tailnet"]["allowed"].clone()), (json!(false), json!(false)));
        assert_eq!(body(&api(&s, &mut g, &session(&ci, false, true)))["authed"], false);

        // Allow-listing and turning it off.
        s.set_setting(TAILNET_ALLOW_KEY, Some("eve@example.com")).unwrap();
        assert_eq!(body(&api(&s, &mut g, &session(&eve, false, true)))["authed"], true);
        s.set_setting(TAILNET_KEY, Some("off")).unwrap();
        assert_eq!(body(&api(&s, &mut g, &session(&jack, false, true)))["authed"], false);
    }

    #[test]
    fn decodes() {
        assert_eq!(percent_decode("vegitarian%20food+now%"), "vegitarian food now%");
        assert_eq!(session_cookie("a=b; recall_session=abc; c=d"), Some("abc"));
        assert!(signed_out("recall_signed_out=1; recall_session=abc"));
        assert!(!signed_out("recall_session=abc"));
    }
}
