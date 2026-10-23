//! HTTP server: MCP at `/mcp` (bearer tokens) and the web UI at `/`
//! (password + session cookie).

use std::io::Read;
use std::net::{IpAddr, SocketAddr};

use serde_json::Value;
use tiny_http::{Header, Request, Response, Server as Http};

use crate::Result;
use crate::mcp::Server;
use crate::web::{self, Reply};

const MAX_BODY: u64 = 64 * 1024;

const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("X-Frame-Options", "DENY"),
];

pub struct Options {
    pub listen: SocketAddr,
    /// Extra browser origins allowed to call (same-origin is always allowed).
    pub allow_origins: Vec<String>,
}

pub fn serve(mut server: Server, opts: Options) -> Result<()> {
    let http = Http::http(opts.listen).map_err(|e| format!("bind {}: {e}", opts.listen))?;
    eprintln!("recall: MCP at http://{}/mcp, web UI at http://{}/", opts.listen, opts.listen);
    let mut guard = web::Guard::default();
    for req in http.incoming_requests() {
        if let Err(e) = handle(&mut server, &mut guard, &opts, req) {
            eprintln!("recall: {e}");
        }
    }
    Ok(())
}

/// DNS-rebinding defence: only answer to names that can legitimately reach
/// this server — loopback names on loopback, otherwise the bound IP,
/// tailnet (`*.ts.net`) and single-label (MagicDNS/LAN) names.
fn host_allowed(host: &str, listen: &SocketAddr) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    }
    .to_ascii_lowercase();
    let loopback = matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1");
    if listen.ip().is_loopback() {
        return loopback;
    }
    if let Ok(ip) = name.parse::<IpAddr>() {
        return listen.ip().is_unspecified() || ip == listen.ip() || ip.is_loopback();
    }
    loopback || name.ends_with(".ts.net") || (!name.is_empty() && !name.contains('.'))
}

fn handle(server: &mut Server, guard: &mut web::Guard, opts: &Options, mut req: Request) -> Result<()> {
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|h| (h.field.as_str().as_str().to_ascii_lowercase(), h.value.as_str().to_string()))
        .collect();
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let method = req.method().as_str().to_string();

    let host = header("host").unwrap_or_default();
    if !host_allowed(&host, &opts.listen) {
        return respond(req, Reply::json(403, serde_json::json!({ "error": "forbidden host" })));
    }
    if let Some(origin) = header("origin")
        && origin != format!("http://{host}")
        && origin != format!("https://{host}")
        && !opts.allow_origins.contains(&origin)
    {
        return respond(req, Reply::json(403, serde_json::json!({ "error": "forbidden origin" })));
    }

    let mut body = Vec::new();
    req.as_reader().take(MAX_BODY + 1).read_to_end(&mut body)?;
    if body.len() as u64 > MAX_BODY {
        return respond(req, Reply::json(413, serde_json::json!({ "error": "body too large" })));
    }

    let reply = match (method.as_str(), path) {
        (_, "/mcp") => mcp(server, &method, &header("authorization"), &header("content-type"), &body)?,
        ("GET", "/" | "/index.html") => asset("text/html; charset=utf-8", web::INDEX_HTML.as_bytes()),
        ("GET", "/app.js") => asset("text/javascript; charset=utf-8", web::APP_JS.as_bytes()),
        ("GET", "/app.css") => asset("text/css; charset=utf-8", web::APP_CSS.as_bytes()),
        ("GET", "/icon.png") => asset("image/png", web::ICON_PNG),
        (_, p) if p.starts_with("/api/") => {
            let cookie = header("cookie");
            let request = web::Request {
                method: &method,
                path,
                query,
                body: &body,
                cookie: cookie.as_deref().and_then(web::session_cookie),
                ajax: header("x-requested-with").as_deref() == Some("recall"),
            };
            let mut r = web::api(&server.store, guard, &request);
            r.headers.push(("Cache-Control", "no-store".into()));
            r
        }
        _ => Reply { status: 404, content_type: "text/plain", body: b"not found".to_vec(), headers: Vec::new() },
    };
    respond(req, reply)
}

fn asset(content_type: &'static str, body: &[u8]) -> Reply {
    Reply { status: 200, content_type, body: body.to_vec(), headers: vec![("Cache-Control", "no-cache".into())] }
}

fn mcp(server: &mut Server, method: &str, auth: &Option<String>, content_type: &Option<String>, body: &[u8]) -> Result<Reply> {
    let text = |status, msg: &str| Reply { status, content_type: "text/plain", body: msg.as_bytes().to_vec(), headers: Vec::new() };
    if method != "POST" {
        let mut r = text(405, "method not allowed");
        r.headers.push(("Allow", "POST".into()));
        return Ok(r);
    }
    let token = auth.as_deref().and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
    let Some((name, scope)) = token.map(|t| server.store.token_check(t)).transpose()?.flatten() else {
        let mut r = text(401, "unauthorized");
        r.headers.push(("WWW-Authenticate", "Bearer realm=\"recall\"".into()));
        return Ok(r);
    };
    if !content_type.as_deref().is_some_and(|c| c.starts_with("application/json")) {
        return Ok(text(415, "expected application/json"));
    }
    let Ok(msg) = serde_json::from_slice::<Value>(body) else {
        return Ok(text(400, "invalid JSON"));
    };
    server.scope = scope;
    server.client = name;
    Ok(match server.handle(&msg) {
        Some(out) => Reply::json(200, out),
        None => Reply { status: 202, content_type: "text/plain", body: Vec::new(), headers: Vec::new() },
    })
}

fn respond(req: Request, reply: Reply) -> Result<()> {
    let mut resp = Response::from_data(reply.body)
        .with_status_code(reply.status)
        .with_header(Header::from_bytes("Content-Type", reply.content_type).expect("static header"));
    for (k, v) in SECURITY_HEADERS {
        resp.add_header(Header::from_bytes(*k, *v).expect("static header"));
    }
    for (k, v) in reply.headers {
        if let Ok(h) = Header::from_bytes(k, v.as_bytes()) {
            resp.add_header(h);
        }
    }
    Ok(req.respond(resp)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        let lo: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let ts: SocketAddr = "100.101.102.103:7879".parse().unwrap();
        assert!(host_allowed("localhost:7878", &lo));
        assert!(!host_allowed("evil.example:7878", &lo));
        assert!(host_allowed("pi5.bandicoot-nominal.ts.net:7879", &ts));
        assert!(host_allowed("pi5:7879", &ts));
        assert!(host_allowed("100.101.102.103:7879", &ts));
        assert!(!host_allowed("evil.example:7879", &ts));
        assert!(!host_allowed("1.2.3.4:7879", &ts));
    }
}
