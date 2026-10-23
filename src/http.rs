//! Streamable HTTP transport (POST /mcp, JSON responses, bearer auth).

use std::io::Read;
use std::net::SocketAddr;

use serde_json::Value;
use tiny_http::{Header, Method, Request, Response, Server as Http};

use crate::Result;
use crate::mcp::Server;

const MAX_BODY: u64 = 64 * 1024;

pub struct Options {
    pub listen: SocketAddr,
    /// Browser origins allowed to call (none by default).
    pub allow_origins: Vec<String>,
}

pub fn serve(mut server: Server, opts: Options) -> Result<()> {
    let http = Http::http(opts.listen).map_err(|e| format!("bind {}: {e}", opts.listen))?;
    eprintln!("recall: listening on http://{}/mcp", opts.listen);
    for req in http.incoming_requests() {
        let resp = handle(&mut server, &opts, req);
        if let Err(e) = resp {
            eprintln!("recall: {e}");
        }
    }
    Ok(())
}

fn handle(server: &mut Server, opts: &Options, mut req: Request) -> Result<()> {
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|h| (h.field.as_str().as_str().to_ascii_lowercase(), h.value.as_str().to_string()))
        .collect();
    let header = |name: &str| {
        let name = name.to_ascii_lowercase();
        headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone())
    };

    if req.url().split('?').next() != Some("/mcp") {
        return Ok(req.respond(text(404, "not found"))?);
    }
    // DNS-rebinding defence: a loopback server only answers loopback Host names,
    // and browser Origins must be explicitly allowed.
    if opts.listen.ip().is_loopback() {
        let host = header("Host").unwrap_or_default();
        let name = host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h);
        if !matches!(name, "localhost" | "127.0.0.1" | "[::1]") {
            return Ok(req.respond(text(403, "forbidden host"))?);
        }
    }
    if let Some(origin) = header("Origin")
        && !opts.allow_origins.contains(&origin)
    {
        return Ok(req.respond(text(403, "forbidden origin"))?);
    }
    if *req.method() != Method::Post {
        let allow = Header::from_bytes("Allow", "POST").expect("static header");
        return Ok(req.respond(text(405, "method not allowed").with_header(allow))?);
    }

    let token = header("Authorization").and_then(|v| v.strip_prefix("Bearer ").map(str::trim).map(String::from));
    let Some((name, scope)) = token.as_deref().map(|t| server.store.token_check(t)).transpose()?.flatten() else {
        let challenge = Header::from_bytes("WWW-Authenticate", "Bearer realm=\"recall\"").expect("static header");
        return Ok(req.respond(text(401, "unauthorized").with_header(challenge))?);
    };

    if !header("Content-Type").is_some_and(|c| c.starts_with("application/json")) {
        return Ok(req.respond(text(415, "expected application/json"))?);
    }
    let mut body = Vec::new();
    req.as_reader().take(MAX_BODY + 1).read_to_end(&mut body)?;
    if body.len() as u64 > MAX_BODY {
        return Ok(req.respond(text(413, "body too large"))?);
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return Ok(req.respond(text(400, "invalid JSON"))?);
    };

    server.scope = scope;
    server.client = name;
    match server.handle(&msg) {
        Some(out) => {
            let ct = Header::from_bytes("Content-Type", "application/json").expect("static header");
            Ok(req.respond(Response::from_data(serde_json::to_vec(&out)?).with_header(ct))?)
        }
        None => Ok(req.respond(Response::empty(202))?),
    }
}

fn text(code: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(code)
}
