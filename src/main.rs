use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::process::ExitCode;

use serde_json::{Value, json};

use recall::config::{self, Key};
use recall::mcp::Server;
use recall::embed;
use recall::store::{self, Mode, Saved, Scope, Store};
use recall::{Result, http};

const USAGE: &str = "\
recall — personal memory for every AI agent, over MCP

USAGE:
  recall init                         create the encrypted store and key
  recall mcp                          run the stdio MCP server (what agents spawn)
  recall serve [--listen ADDR] [--allow-origin ORIGIN]...
                                      run the HTTP MCP server (default 127.0.0.1:7878)
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
  RECALL_KEY_CMD   command that prints the 64-hex-char key (e.g. from a password manager)";

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
        ["mcp"] => stdio(Server::new(open()?, Scope::Write))?,
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

fn init() -> Result<()> {
    let home = config::ensure_home()?;
    let db = config::db_path()?;
    if std::env::var_os("RECALL_KEY_CMD").is_none() && Key::load().is_err() {
        if db.exists() {
            return Err(format!("{} exists but no key was found; refusing to create a new key", db.display()).into());
        }
        let path = Key::generate()?.save()?;
        println!("Key written to {} (0600). Back it up: without it the store is unreadable.", path.display());
    }
    let s = open()?;
    println!("Store ready at {} ({} memories).", db.display(), s.count()?);
    println!("\nAdd to an MCP client, e.g. Claude Code:\n  claude mcp add --scope user recall -- {} mcp", std::env::current_exe()?.display());
    let _ = home;
    Ok(())
}

fn stdio(mut server: Server) -> Result<()> {
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
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
    let mut allow_origins = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--listen" => listen = it.next().ok_or("--listen needs ADDR")?.parse()?,
            "--allow-origin" => allow_origins.push(it.next().ok_or("--allow-origin needs ORIGIN")?.to_string()),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let store = open()?;
    if store.token_list()?.is_empty() {
        eprintln!("recall: no tokens yet; create one with `recall token add NAME`");
    }
    if !listen.ip().is_loopback() {
        eprintln!("recall: WARNING listening on a non-loopback address; put TLS in front (Tailscale, Caddy, Cloudflare Tunnel)");
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
