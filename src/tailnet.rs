//! Sign-in by Tailscale identity.
//!
//! When Recall runs on a tailnet device, the local Tailscale daemon can say
//! which user owns the device a connection comes from (`tailscale whois`),
//! the same cryptographically verified identity tsidp is built on. The web UI
//! uses it to sign people in without a password.

use std::net::{IpAddr, SocketAddr};
use std::process::{Command, Stdio};

use serde_json::Value;

/// The person behind a tailnet connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    /// e.g. `jack@example.com` or `jack@github`.
    pub login: String,
    pub name: String,
    /// Tagged devices (servers, CI) belong to no user and never sign in.
    pub tagged: bool,
}

/// What `recall serve` learns about the tailnet at startup.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// Login of the user who owns this device (none if the device is tagged).
    pub owner: Option<String>,
    /// `tailscale serve` proxies to this server, so its identity headers
    /// (`Tailscale-User-Login`) on loopback connections can be trusted.
    pub behind_serve: bool,
}

fn cli(args: &[&str]) -> Option<Vec<u8>> {
    for bin in ["tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"] {
        if let Ok(out) = Command::new(bin).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output()
            && out.status.success()
        {
            return Some(out.stdout);
        }
    }
    None
}

fn json(args: &[&str]) -> Option<Value> {
    serde_json::from_slice(&cli(args)?).ok()
}

/// Tailscale's address ranges: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64,
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Inspect the local Tailscale daemon. `None` if Tailscale isn't running.
pub fn detect(port: u16) -> Option<Info> {
    let status = json(&["status", "--json"])?;
    if status["BackendState"] != "Running" {
        return None;
    }
    let serve = json(&["serve", "status", "--json"]).unwrap_or(Value::Null);
    Some(Info { owner: owner(&status), behind_serve: proxied_ports(&serve).contains(&port) })
}

/// Who is connecting from `peer`, according to the Tailscale daemon.
pub fn whois(peer: SocketAddr) -> Option<Identity> {
    if !is_tailnet_ip(peer.ip()) {
        return None;
    }
    parse_whois(&json(&["whois", "--json", &peer.to_string()])?)
}

fn owner(status: &Value) -> Option<String> {
    let me = &status["Self"];
    if me["Tags"].as_array().is_some_and(|t| !t.is_empty()) {
        return None;
    }
    let uid = me["UserID"].as_u64()?.to_string();
    status["User"][&uid]["LoginName"].as_str().map(str::to_string)
}

fn parse_whois(v: &Value) -> Option<Identity> {
    let profile = &v["UserProfile"];
    let login = profile["LoginName"].as_str()?.to_string();
    let tagged = v["Node"]["Tags"].as_array().is_some_and(|t| !t.is_empty()) || login == "tagged-devices";
    let name = profile["DisplayName"].as_str().unwrap_or(&login).to_string();
    Some(Identity { login, name, tagged })
}

/// Local ports that `tailscale serve` proxies web traffic to.
fn proxied_ports(serve: &Value) -> Vec<u16> {
    let mut ports = Vec::new();
    for site in serve["Web"].as_object().into_iter().flat_map(|w| w.values()) {
        for handler in site["Handlers"].as_object().into_iter().flat_map(|h| h.values()) {
            let Some(target) = handler["Proxy"].as_str() else { continue };
            let rest = target.trim_start_matches("http://").trim_start_matches("https://");
            let (host, port) = rest.split('/').next().unwrap_or("").rsplit_once(':').unwrap_or((rest, ""));
            if matches!(host, "127.0.0.1" | "localhost" | "[::1]")
                && let Ok(p) = port.parse()
            {
                ports.push(p);
            }
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tailnet_ranges() {
        assert!(is_tailnet_ip("100.101.102.103".parse().unwrap()));
        assert!(is_tailnet_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("192.168.1.10".parse().unwrap()));
        assert!(is_tailnet_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailnet_ip("::1".parse().unwrap()));
    }

    #[test]
    fn parses_daemon_json() {
        let status = json!({ "BackendState": "Running", "Self": { "UserID": 42 }, "User": { "42": { "LoginName": "jack@example.com" } } });
        assert_eq!(owner(&status).as_deref(), Some("jack@example.com"));
        let tagged = json!({ "Self": { "UserID": 42, "Tags": ["tag:server"] }, "User": { "42": { "LoginName": "x" } } });
        assert_eq!(owner(&tagged), None);

        let who = json!({ "Node": { "Name": "phone." }, "UserProfile": { "LoginName": "jack@example.com", "DisplayName": "Jack" } });
        assert_eq!(parse_whois(&who), Some(Identity { login: "jack@example.com".into(), name: "Jack".into(), tagged: false }));
        let ci = json!({ "Node": { "Tags": ["tag:ci"] }, "UserProfile": { "LoginName": "tagged-devices" } });
        assert!(parse_whois(&ci).unwrap().tagged);

        let serve = json!({ "Web": { "pi5.x.ts.net:443": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:7878" } } } } });
        assert_eq!(proxied_ports(&serve), vec![7878]);
        assert!(proxied_ports(&Value::Null).is_empty());
    }
}
