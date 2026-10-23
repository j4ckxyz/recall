use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};

use recall::config::{self, Key};
use recall::mcp::Server;
use recall::embed;
use recall::store::{self, Mode, Saved, Scope, Store};
use recall::remote::{self, Remote};
use recall::setup;
use recall::{Result, http};

const USAGE: &str = "\
recall — personal memory for every AI agent, over MCP

USAGE:
  recall init                         create the encrypted store and key
  recall connect [AGENT...]           add Recall to installed agents (default: all detected)
  recall disconnect [AGENT...]        remove it again
  recall doctor                       self-test everything
  recall mcp                          run the stdio MCP server (what agents spawn)
  recall serve [--tailscale | --listen ADDR] [--port N] [--allow-origin ORIGIN]...
                                      run the HTTP MCP server (default 127.0.0.1:7878;
                                      --tailscale listens on this device's tailnet IP)
  recall password [--set | --off]     generate the web UI password (--set: read yours from stdin)
  recall remote URL TOKEN             use a Recall server elsewhere (e.g. http://pi:7878/mcp)
  recall remote | remote off          show / stop using the remote server
  recall token add NAME [--read-only] create an HTTP bearer token (shown once)
  recall token list | revoke NAME
  recall list                         show every memory
  recall search QUERY                 search memories
  recall add TEXT [--date DATE]       save a memory (DATE: when it was true, YYYY-MM-DD)
  recall rm ID | restore ID           forget / undo forget
  recall trash                        show forgotten memories (kept 30 days)
  recall purge [--all]                hard-delete forgotten memories (--all: now)
  recall audit [N]                    which agent wrote what, when
  recall export [--json]              print everything (markdown or JSON)
  recall import FILE                  import a JSON export or one fact per line

ENVIRONMENT:
  RECALL_HOME      state directory (default ~/.recall)
  RECALL_KEY_CMD   command that prints the 64-hex-char key (e.g. from a password manager)
  RECALL_REMOTE_URL, RECALL_REMOTE_TOKEN
                   use a remote server (overrides `recall remote`)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("recall: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open() -> Result<Store> {
    Store::open(&config::db_path()?, &Key::load()?, embed::bundled())
}

fn run(args: &[String]) -> Result<()> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] | ["help" | "-h" | "--help"] => println!("{USAGE}"),
        ["version" | "-V" | "--version"] => println!("recall {}", env!("CARGO_PKG_VERSION")),
        ["init"] => init()?,
        ["doctor"] => setup::doctor()?,
        ["connect", which @ ..] => connect(which, true)?,
        ["disconnect", which @ ..] => connect(which, false)?,
        ["mcp"] => match Remote::load()? {
            Some(r) => r.relay()?,
            None => {
                // First launch (e.g. installed from the MCP Registry or as a
                // Claude Desktop extension): create the store, no terminal needed.
                let (store, new_key) = ensure_store()?;
                if let Some(k) = new_key {
                    eprintln!("recall: created an encrypted memory store; back up {}", k.display());
                }
                stdio(Server::new(store, Scope::Write))?
            }
        },
        ["password", rest @ ..] => password(rest)?,
        ["remote"] => match Remote::load()? {
            Some(r) => println!("Using remote server {}", r.url()),
            None => println!("Using the local store (no remote configured)."),
        },
        ["remote", "off"] => {
            let _ = std::fs::remove_file(config::remote_path()?);
            println!("Remote disabled; agents use the local store.");
        }
        ["remote", url, token] => {
            let r = Remote::new(url, token)?;
            r.send(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))?;
            Remote::save(url, token)?;
            println!("Connected to {}. Every agent on this device now uses it.", r.url());
        }
        ["serve", rest @ ..] => serve(rest)?,
        ["token", "add", name, rest @ ..] => {
            let scope = if rest.contains(&"--read-only") { Scope::Read } else { Scope::Write };
            let t = open()?.token_add(name, scope)?;
            println!("{t}\n\nStore this now; it cannot be shown again.");
        }
        ["token", "list"] => {
            for (name, scope) in open()?.token_list()? {
                println!("{name}\t{scope}");
            }
        }
        ["token", "revoke", name] => found(open()?.token_revoke(name)?, name)?,
        ["list"] => {
            for m in open()?.all()? {
                println!("[{}] {} {}", m.sid, m.at, m.content);
            }
        }
        ["search", q @ ..] => {
            for m in open()?.search(&q.join(" "), 20, Mode::Hybrid)? {
                println!("[{}] {} {}", m.sid, m.at, m.content);
            }
        }
        ["add", rest @ ..] => {
            let (date, words) = take_flag(rest, "--date");
            match open()?.remember(&words.join(" "), date, None, "owner")? {
                Saved::New { sid, related } => {
                    println!("Saved [{sid}].");
                    for m in related {
                        println!("  related: [{}] {} {}", m.sid, m.at, m.content);
                    }
                }
                Saved::Duplicate(m) => println!("Already known [{}].", m.sid),
                Saved::Replaced { .. } => unreachable!("no replaces given"),
            }
        }
        ["rm", sid] => found(open()?.forget(sid, "owner")?, sid)?,
        ["restore", sid] => found(open()?.restore(sid)?, sid)?,
        ["trash"] => {
            for m in open()?.trash()? {
                println!("[{}] {}", m.sid, m.content);
            }
        }
        ["purge", rest @ ..] => {
            let age = if rest.contains(&"--all") { 0 } else { store::RETENTION_SECS };
            println!("Purged {} memories.", open()?.purge(age)?);
        }
        ["audit", rest @ ..] => {
            let n = rest.first().and_then(|s| s.parse().ok()).unwrap_or(50);
            for (ts, op, sid, client) in open()?.audit_log(n)? {
                println!("{}\t{op}\t{sid}\t{client}", recall::when::format_unix(ts));
            }
        }
        ["export", rest @ ..] => export(&open()?, rest.contains(&"--json"))?,
        ["import", file] => import(&open()?, file)?,
        _ => return Err("unknown command; see `recall help`".into()),
    }
    Ok(())
}

fn found(ok: bool, what: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(format!("not found: {what}").into()) }
}

fn take_flag<'a>(args: &[&'a str], flag: &str) -> (Option<&'a str>, Vec<&'a str>) {
    let mut value = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if *a == flag {
            value = it.next().copied();
        } else {
            rest.push(*a);
        }
    }
    (value, rest)
}

/// Create the key if there is none (and no store it would orphan), then open
/// the store. Returns the new key's path when one was created.
fn ensure_store() -> Result<(Store, Option<PathBuf>)> {
    config::ensure_home()?;
    let db = config::db_path()?;
    let mut new_key = None;
    if std::env::var_os("RECALL_KEY_CMD").is_none() && !config::key_path()?.exists() {
        if db.exists() {
            return Err(format!("{} exists but its key is missing; restore ~/.recall/key from your backup", db.display()).into());
        }
        new_key = Some(Key::generate()?.save()?);
    }
    Ok((open()?, new_key))
}

fn init() -> Result<()> {
    let (store, new_key) = ensure_store()?;
    if let Some(k) = new_key {
        println!("Key written to {} (owner-only). Back it up: without it the store is unreadable.", k.display());
    }
    println!("Store ready at {} ({} memories).", config::db_path()?.display(), store.count()?);
    println!("\nNext: `recall connect` adds Recall to your installed agents; `recall doctor` checks everything.");
    Ok(())
}

fn stdio(mut server: Server) -> Result<()> {
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        // Windows PowerShell prefixes piped text with a UTF-8 BOM.
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(msg) => server.handle(&msg),
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

fn serve(args: &[&str]) -> Result<()> {
    let mut listen: SocketAddr = "127.0.0.1:7878".parse()?;
    let mut port = None;
    let mut allow_origins = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--listen" => listen = it.next().ok_or("--listen needs ADDR")?.parse()?,
            "--tailscale" => listen = format!("{}:7878", remote::tailscale_ip()?).parse()?,
            "--port" => port = Some(it.next().ok_or("--port needs N")?.parse::<u16>()?),
            "--allow-origin" => allow_origins.push(it.next().ok_or("--allow-origin needs ORIGIN")?.to_string()),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    if let Some(p) = port {
        listen.set_port(p);
    }
    let store = open()?;
    if store.token_list()?.is_empty() {
        eprintln!("recall: no MCP tokens yet; create one with `recall token add NAME`");
    }
    if store.setting(recall::web::PASSWORD_KEY)?.is_none() {
        eprintln!("recall: no web UI password yet; create one with `recall password`");
    }
    let tailnet = matches!(listen.ip(), std::net::IpAddr::V4(ip) if ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]));
    if !listen.ip().is_loopback() && !tailnet {
        eprintln!("recall: WARNING listening on a non-loopback, non-tailnet address; traffic is unencrypted. Prefer --tailscale.");
    }
    http::serve(Server::new(store, Scope::Read), http::Options { listen, allow_origins })
}

fn export(s: &Store, as_json: bool) -> Result<()> {
    let all = s.all()?;
    if as_json {
        let v: Vec<Value> = all.iter().map(|m| json!({ "id": m.sid, "date": m.at, "content": m.content })).collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        for m in all {
            println!("- {} {}", m.at, m.content);
        }
    }
    Ok(())
}

/// Import a `recall export --json` file, or plain text with one fact per line
/// (e.g. memories copied out of ChatGPT or Claude).
fn import(s: &Store, file: &str) -> Result<()> {
    let data = std::fs::read_to_string(file)?;
    let items: Vec<(String, Option<String>)> = if data.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<Value>>(&data)?
            .into_iter()
            .filter_map(|v| {
                let c = v.get("content")?.as_str()?.to_string();
                Some((c, v.get("date").and_then(Value::as_str).map(String::from)))
            })
            .collect()
    } else {
        data.lines()
            .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| (l, None))
            .collect()
    };
    let (mut added, mut skipped) = (0, 0);
    for (content, date) in items {
        match s.remember(&content, date.as_deref(), None, "import") {
            Ok(Saved::New { .. }) => added += 1,
            Ok(_) => skipped += 1,
            Err(e) => {
                skipped += 1;
                eprintln!("skipped: {e}");
            }
        }
    }
    println!("Imported {added}, skipped {skipped} (duplicates or rejected).");
    Ok(())
}

/// `connect` / `disconnect` the named agents, or all detected/connected ones.
fn connect(which: &[&str], add: bool) -> Result<()> {
    let all = setup::clients()?;
    for w in which {
        if !all.iter().any(|c| c.id == *w) {
            let ids: Vec<&str> = all.iter().map(|c| c.id).collect();
            return Err(format!("unknown agent '{w}'; known: {}", ids.join(", ")).into());
        }
    }
    let chosen: Vec<_> = all
        .iter()
        .filter(|c| if which.is_empty() { if add { c.detected } else { c.is_connected() } } else { which.contains(&c.id) })
        .collect();
    if chosen.is_empty() {
        println!("No {} agents found. Supported: {}.", if add { "installed" } else { "connected" }, all.iter().map(|c| c.name).collect::<Vec<_>>().join(", "));
        return Ok(());
    }
    let mut failed = false;
    for c in chosen {
        let result = if add {
            if c.is_connected() { Ok("already connected".to_string()) } else { c.connect() }
        } else {
            c.disconnect().map(|()| "removed".to_string())
        };
        match result {
            Ok(m) => println!("  ✓ {:<15} {m}", c.name),
            Err(e) => {
                failed = true;
                println!("  ✗ {:<15} {e}", c.name);
            }
        }
    }
    if add {
        println!("\nRestart open agents to pick up the change.");
    }
    if failed { Err("some agents could not be updated".into()) } else { Ok(()) }
}

/// Set the web UI password. Changing it signs out every browser.
fn password(args: &[&str]) -> Result<()> {
    use recall::{auth, web::PASSWORD_KEY};
    let store = open()?;
    match args {
        ["--off"] => {
            store.set_setting(PASSWORD_KEY, None)?;
            store.session_remove(None)?;
            println!("Web UI sign-in disabled.");
        }
        ["--set"] => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let pw = line.trim_end_matches(['\r', '\n']);
            if pw.chars().count() < 12 {
                return Err("use at least 12 characters".into());
            }
            store.set_setting(PASSWORD_KEY, Some(&auth::hash_password(pw, auth::ITERATIONS)?))?;
            store.session_remove(None)?;
            println!("Web UI password set.");
        }
        [] => {
            let pw = auth::generate_password()?;
            store.set_setting(PASSWORD_KEY, Some(&auth::hash_password(&pw, auth::ITERATIONS)?))?;
            store.session_remove(None)?;
            println!("{pw}\n\nWeb UI password (shown once). Open the server's address in a browser, e.g. http://localhost:7878/");
        }
        _ => return Err("usage: recall password [--set | --off]".into()),
    }
    Ok(())
}
