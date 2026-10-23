//! `recall connect` (register Recall with installed agents) and `recall doctor`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value, json};

use crate::config::{self, user_home};
use crate::remote::Remote;
use crate::store::{Mode, Saved, Store};
use crate::{Result, embed};

/// How an agent stores its MCP configuration.
enum Kind {
    /// `claude mcp add` CLI.
    ClaudeCli,
    /// JSON file with servers under `key`; `typed` adds `"type": "stdio"`.
    Json { path: PathBuf, key: &'static str, typed: bool },
    /// Codex `config.toml`.
    Toml { path: PathBuf },
}

pub struct Client {
    pub id: &'static str,
    pub name: &'static str,
    kind: Kind,
    /// The agent looks installed.
    pub detected: bool,
}

/// Per-OS application config directory (`~/Library/Application Support`,
/// `%APPDATA%`, `~/.config`).
fn app_config() -> Result<PathBuf> {
    let home = user_home()?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Roaming"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"))
    })
}

pub fn clients() -> Result<Vec<Client>> {
    let home = user_home()?;
    let app = app_config()?;
    let json = |path: PathBuf, key, typed| Kind::Json { path, key, typed };
    let mut out = vec![
        Client { id: "claude-code", name: "Claude Code", kind: Kind::ClaudeCli, detected: cli_ok(&["--version"]) },
        Client {
            id: "claude-desktop",
            name: "Claude Desktop",
            detected: app.join("Claude").is_dir(),
            kind: json(app.join("Claude/claude_desktop_config.json"), "mcpServers", false),
        },
        Client { id: "cursor", name: "Cursor", detected: home.join(".cursor").is_dir(), kind: json(home.join(".cursor/mcp.json"), "mcpServers", false) },
        Client {
            id: "vscode",
            name: "VS Code",
            detected: app.join("Code/User").is_dir(),
            kind: json(app.join("Code/User/mcp.json"), "servers", true),
        },
        Client {
            id: "windsurf",
            name: "Windsurf",
            detected: home.join(".codeium/windsurf").is_dir(),
            kind: json(home.join(".codeium/windsurf/mcp_config.json"), "mcpServers", false),
        },
        Client { id: "gemini", name: "Gemini CLI", detected: home.join(".gemini").is_dir(), kind: json(home.join(".gemini/settings.json"), "mcpServers", false) },
        Client { id: "codex", name: "Codex CLI", detected: home.join(".codex").is_dir(), kind: Kind::Toml { path: home.join(".codex/config.toml") } },
    ];
    out.sort_by_key(|c| !c.detected);
    Ok(out)
}

/// Run the `claude` CLI (a `.cmd` shim on Windows, so go through `cmd`).
fn claude(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "claude"]);
        c
    } else {
        Command::new("claude")
    };
    cmd.args(args).stdin(Stdio::null()).output()
}

fn cli_ok(args: &[&str]) -> bool {
    claude(args).is_ok_and(|o| o.status.success())
}

fn exe() -> Result<String> {
    let p = std::env::current_exe()?;
    Ok(fs::canonicalize(&p).unwrap_or(p).to_string_lossy().trim_start_matches(r"\\?\").to_string())
}

impl Client {
    pub fn is_connected(&self) -> bool {
        match &self.kind {
            Kind::ClaudeCli => self.detected && cli_ok(&["mcp", "get", "recall"]),
            Kind::Json { path, key, .. } => read_json(path).ok().flatten().is_some_and(|v| v[key].get("recall").is_some()),
            Kind::Toml { path } => fs::read_to_string(path).is_ok_and(|s| s.contains("[mcp_servers.recall]")),
        }
    }

    pub fn connect(&self) -> Result<String> {
        let exe = exe()?;
        match &self.kind {
            Kind::ClaudeCli => {
                let out = claude(&["mcp", "add", "--scope", "user", "recall", "--", &exe, "mcp"])?;
                if !out.status.success() {
                    return Err(String::from_utf8_lossy(&out.stderr).trim().to_string().into());
                }
                Ok("added with `claude mcp add --scope user`".into())
            }
            Kind::Json { path, key, typed } => {
                let mut v = read_json(path)?.unwrap_or_else(|| json!({}));
                let obj = v.as_object_mut().ok_or("config is not a JSON object")?;
                let servers = obj.entry(*key).or_insert_with(|| json!({}));
                let mut entry = Map::new();
                if *typed {
                    entry.insert("type".into(), json!("stdio"));
                }
                entry.insert("command".into(), json!(exe));
                entry.insert("args".into(), json!(["mcp"]));
                servers.as_object_mut().ok_or(format!("`{key}` is not an object"))?.insert("recall".into(), Value::Object(entry));
                write_with_backup(path, (serde_json::to_string_pretty(&v)? + "\n").as_bytes())?;
                Ok(format!("wrote {}", path.display()))
            }
            Kind::Toml { path } => {
                let old = fs::read_to_string(path).unwrap_or_default();
                if old.contains("[mcp_servers.recall]") {
                    return Ok("already configured".into());
                }
                // TOML literal strings: no escaping needed for Windows paths.
                let block = format!("\n\n[mcp_servers.recall]\ncommand = '{exe}'\nargs = ['mcp']\n");
                write_with_backup(path, format!("{}{block}", old.trim_end()).trim_start().as_bytes())?;
                Ok(format!("wrote {}", path.display()))
            }
        }
    }

    pub fn disconnect(&self) -> Result<()> {
        match &self.kind {
            Kind::ClaudeCli => {
                claude(&["mcp", "remove", "--scope", "user", "recall"])?;
            }
            Kind::Json { path, key, .. } => {
                if let Some(mut v) = read_json(path)?
                    && let Some(s) = v.get_mut(*key).and_then(Value::as_object_mut)
                    && s.remove("recall").is_some()
                {
                    write_with_backup(path, (serde_json::to_string_pretty(&v)? + "\n").as_bytes())?;
                }
            }
            Kind::Toml { path } => {
                let old = fs::read_to_string(path).unwrap_or_default();
                let mut skip = false;
                let kept: Vec<&str> = old
                    .lines()
                    .filter(|l| {
                        let t = l.trim();
                        if t.starts_with('[') {
                            skip = t == "[mcp_servers.recall]";
                        }
                        !skip
                    })
                    .collect();
                if kept.len() != old.lines().count() {
                    write_with_backup(path, (kept.join("\n").trim_end().to_string() + "\n").as_bytes())?;
                }
            }
        }
        Ok(())
    }
}

/// `Ok(None)` if the file doesn't exist; an error if it isn't plain JSON
/// (we never rewrite a file we can't parse, e.g. JSON with comments).
fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => Ok(Some(serde_json::from_str(&s).map_err(|e| format!("{} is not plain JSON ({e}); add Recall by hand", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Keep one pristine backup (`<file>.before-recall`) the first time we edit.
fn write_with_backup(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let backup = PathBuf::from(format!("{}.before-recall", path.display()));
    if path.exists() && !backup.exists() {
        fs::copy(path, &backup)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

// ---- doctor ----------------------------------------------------------------

struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, what: &str, r: Result<String>) {
        match r {
            Ok(detail) => println!("  ✓ {what}{}", if detail.is_empty() { String::new() } else { format!(" — {detail}") }),
            Err(e) => {
                self.failed = true;
                println!("  ✗ {what} — {e}");
            }
        }
    }
}

/// End-to-end self-test. Never writes to the real store.
pub fn doctor() -> Result<()> {
    println!("recall {} ({}-{})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    let mut r = Report { failed: false };

    r.check("embedding model", (|| {
        let e = embed::bundled().ok_or("not compiled in (build with --features model)")?;
        let cos = embed::dot(&e.embed("User is vegetarian."), &e.embed("diet: does not eat meat"));
        if cos < 0.3 {
            return Err(format!("unexpected similarity {cos:.2}").into());
        }
        Ok(format!("{}-dim, related-sentence similarity {cos:.2}", e.dim()))
    })());

    r.check("memory engine (in-memory test store)", (|| {
        let store = Store::open_in_memory(embed::bundled())?;
        let Saved::New { sid, .. } = store.remember("User has a greyhound named Biscuit.", Some("2024-01"), None, "doctor")? else {
            return Err("save failed".into());
        };
        if store.search("dog", 3, Mode::Hybrid)?.first().map(|m| &m.sid) != Some(&sid) {
            return Err("search did not find a saved memory".into());
        }
        if store.remember("api key = sk-abcdefghijklmnopqrstuvwxyz", None, None, "doctor").is_ok() {
            return Err("secret scanner let a key through".into());
        }
        store.forget(&sid, "doctor")?;
        Ok("save, fuzzy search, secret filter, forget".into())
    })());

    match Remote::load() {
        Ok(Some(remote)) => r.check("remote server", (|| {
            let reply = remote.send(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))?.ok_or("no reply")?;
            let n = reply.pointer("/result/tools").and_then(Value::as_array).map_or(0, Vec::len);
            Ok(format!("{} ({n} tools)", remote.url()))
        })()),
        Ok(None) => r.check("encrypted store", (|| {
            let s = Store::open(&config::db_path()?, &config::Key::load()?, embed::bundled())?;
            Ok(format!("{} ({} memories)", config::db_path()?.display(), s.count()?))
        })()),
        Err(e) => r.check("remote settings", Err(e)),
    }

    r.check("MCP over stdio (spawns `recall mcp`)", stdio_handshake());

    println!("\nAgents:");
    for c in clients()? {
        let state = match (c.detected, c.is_connected()) {
            (_, true) => "connected",
            (true, false) => "installed, not connected — run `recall connect`",
            (false, false) => "not found",
        };
        println!("  {:<15} {state}", c.name);
    }
    if r.failed {
        return Err("some checks failed".into());
    }
    println!("\nAll checks passed.");
    Ok(())
}

/// Spawn this binary as an agent would and complete an MCP handshake.
fn stdio_handshake() -> Result<String> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    for msg in [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "recall-doctor", "version": "0" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    ] {
        writeln!(stdin, "{msg}")?;
    }
    drop(stdin);
    let out = child.wait_with_output()?;
    let lines: Vec<Value> = BufReader::new(&out.stdout[..]).lines().map_while(|l| l.ok()).filter_map(|l| serde_json::from_str(&l).ok()).collect();
    let version = lines.first().and_then(|v| v.pointer("/result/protocolVersion")).and_then(Value::as_str);
    let tools: Vec<&str> = lines
        .get(1)
        .and_then(|v| v.pointer("/result/tools"))
        .and_then(Value::as_array)
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    match version {
        Some(v) if !tools.is_empty() => Ok(format!("protocol {v}, tools: {}", tools.join(", "))),
        _ => Err(format!("no valid reply: {}", String::from_utf8_lossy(&out.stderr).trim()).into()),
    }
}

