//! Remote mode: `recall mcp` relays stdio to a Recall server elsewhere in your
//! tailnet, so every agent on every device shares one memory.
//!
//! Plain HTTP is deliberate: inside a tailnet, traffic between devices is
//! already encrypted and authenticated by WireGuard, and skipping TLS keeps
//! the binary free of a TLS stack. For HTTPS (e.g. `tailscale serve`), point
//! agents at the URL directly instead.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::{Value, json};

use crate::Result;
use crate::config;

const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Remote {
    host: String,
    port: u16,
    path: String,
    token: String,
}

impl Remote {
    pub fn new(url: &str, token: &str) -> Result<Self> {
        let rest = url.strip_prefix("http://").ok_or(
            "remote URL must start with http:// (tailnet traffic is already WireGuard-encrypted; for https, configure agents with the URL directly)",
        )?;
        let (authority, path) = rest.split_once('/').map_or((rest, "/mcp".to_string()), |(a, p)| (a, format!("/{p}")));
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "bad port in remote URL")?),
            None => (authority.to_string(), 80),
        };
        if !token.starts_with("rcl_") {
            return Err("token should start with rcl_ (create one on the server with `recall token add NAME`)".into());
        }
        Ok(Remote { host, port, path, token: token.to_string() })
    }

    /// The remote server, if configured: `RECALL_REMOTE_URL` and
    /// `RECALL_REMOTE_TOKEN` (empty means unset), else `remote.json`.
    pub fn load() -> Result<Option<Self>> {
        let env = |k| std::env::var(k).ok().filter(|v: &String| !v.trim().is_empty());
        if let Some(url) = env("RECALL_REMOTE_URL") {
            let token = env("RECALL_REMOTE_TOKEN").ok_or("RECALL_REMOTE_URL is set but RECALL_REMOTE_TOKEN is empty")?;
            return Ok(Some(Remote::new(url.trim(), token.trim())?));
        }
        let path = config::remote_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let field = |k: &str| v[k].as_str().map(str::to_string).ok_or(format!("{} is missing `{k}`", path.display()));
        Ok(Some(Remote::new(&field("url")?, &field("token")?)?))
    }

    pub fn save(url: &str, token: &str) -> Result<()> {
        Remote::new(url, token)?;
        config::ensure_home()?;
        let body = serde_json::to_vec_pretty(&json!({ "url": url, "token": token }))?;
        config::write_private(&config::remote_path()?, &body)
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.path)
    }

    /// POST one JSON-RPC message. `Ok(None)` for an accepted notification.
    pub fn send(&self, msg: &Value) -> Result<Option<Value>> {
        let body = serde_json::to_vec(msg)?;
        let addr = (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| format!("cannot resolve {}", self.host))?;
        let mut s = TcpStream::connect_timeout(&addr, TIMEOUT)?;
        s.set_read_timeout(Some(TIMEOUT))?;
        s.set_write_timeout(Some(TIMEOUT))?;
        write!(
            s,
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.path,
            self.host,
            self.port,
            self.token,
            body.len()
        )?;
        s.write_all(&body)?;

        let mut r = BufReader::new(s);
        let mut status = String::new();
        r.read_line(&mut status)?;
        let code: u16 = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("bad HTTP response")?;
        let mut len = None;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line)? == 0 || line == "\r\n" {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse::<usize>().ok();
            }
        }
        let mut body = Vec::new();
        match len {
            Some(n) => r.take(n as u64).read_to_end(&mut body)?,
            None => r.read_to_end(&mut body)?,
        };
        match code {
            200 => Ok(Some(serde_json::from_slice(&body)?)),
            202 => Ok(None),
            401 => Err("remote rejected the token (401); check `recall remote` settings".into()),
            _ => Err(format!("remote returned HTTP {code}: {}", String::from_utf8_lossy(&body).trim()).into()),
        }
    }

    /// Relay stdin to the remote server until EOF.
    pub fn relay(&self) -> Result<()> {
        let stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        for line in stdin.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => match self.send(&msg) {
                    Ok(r) => r,
                    // Answer requests (not notifications) so the agent doesn't hang.
                    Err(e) => msg.get("id").filter(|_| msg.get("method").is_some()).map(|id| {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": format!("recall remote: {e}") } })
                    }),
                },
                Err(_) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } })),
            };
            if let Some(r) = reply {
                serde_json::to_writer(&mut stdout, &r)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

/// This device's Tailscale IPv4 address, via the `tailscale` CLI.
pub fn tailscale_ip() -> Result<String> {
    let candidates = ["tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"];
    for bin in candidates {
        if let Ok(out) = std::process::Command::new(bin).args(["ip", "-4"]).output()
            && out.status.success()
        {
            let ip = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
            if !ip.is_empty() {
                return Ok(ip);
            }
        }
    }
    Err("could not get a Tailscale IP; is Tailscale installed and logged in? (`tailscale up`)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let r = Remote::new("http://pi.tail1234.ts.net:7878/mcp", "rcl_x").unwrap();
        assert_eq!((r.host.as_str(), r.port, r.path.as_str()), ("pi.tail1234.ts.net", 7878, "/mcp"));
        assert_eq!(Remote::new("http://100.64.0.1:7878", "rcl_x").unwrap().path, "/mcp");
        assert!(Remote::new("https://x", "rcl_x").is_err());
        assert!(Remote::new("http://x", "nope").is_err());
    }
}
