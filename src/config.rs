//! Locations on disk and key loading.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;

use crate::Result;

/// Root directory for all Recall state. `RECALL_HOME` overrides `~/.recall`.
pub fn home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("RECALL_HOME") {
        return Ok(PathBuf::from(h));
    }
    let base = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(base).join(".recall"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(home()?.join("recall.db"))
}

fn key_path() -> Result<PathBuf> {
    Ok(home()?.join("key"))
}

/// A 256-bit SQLCipher key, held as 64 lowercase hex chars.
pub struct Key(String);

impl Key {
    pub fn generate() -> Result<Self> {
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| format!("rng: {e}"))?;
        Ok(Key(hex(&raw)))
    }

    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("key must be 64 hex characters (256 bits)".into());
        }
        Ok(Key(s.to_ascii_lowercase()))
    }

    pub fn hex(&self) -> &str {
        &self.0
    }

    /// Load the key: `RECALL_KEY_CMD` (stdout of a shell command, e.g. a
    /// password manager) wins, otherwise the 0600 key file.
    pub fn load() -> Result<Self> {
        if let Ok(cmd) = std::env::var("RECALL_KEY_CMD") {
            let out = Command::new("/bin/sh").arg("-c").arg(&cmd).output()?;
            if !out.status.success() {
                return Err("RECALL_KEY_CMD failed".into());
            }
            return Key::parse(&String::from_utf8_lossy(&out.stdout));
        }
        let path = key_path()?;
        let meta = fs::metadata(&path)
            .map_err(|_| format!("no key at {} — run `recall init`", path.display()))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!("{} is readable by others; run chmod 600", path.display()).into());
        }
        Key::parse(&fs::read_to_string(&path)?)
    }

    /// Write the key file with mode 0600. Refuses to overwrite.
    pub fn save(&self) -> Result<PathBuf> {
        let path = key_path()?;
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        writeln!(f, "{}", self.0)?;
        Ok(path)
    }
}

/// Create the home directory with mode 0700 (and tighten it if it exists).
pub fn ensure_home() -> Result<PathBuf> {
    let h = home()?;
    fs::create_dir_all(&h)?;
    fs::set_permissions(&h, fs::Permissions::from_mode(0o700))?;
    Ok(h)
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}
