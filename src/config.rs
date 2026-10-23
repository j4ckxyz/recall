//! Locations on disk, private file creation and key loading.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;

/// The user's home directory (`HOME`, or `USERPROFILE` on Windows).
pub fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot find the home directory (HOME/USERPROFILE unset)".into())
}

/// Root directory for all Recall state. `RECALL_HOME` overrides `~/.recall`.
pub fn home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("RECALL_HOME") {
        return Ok(PathBuf::from(h));
    }
    Ok(user_home()?.join(".recall"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(home()?.join("recall.db"))
}

pub fn key_path() -> Result<PathBuf> {
    Ok(home()?.join("key"))
}

/// Settings for using a remote Recall server instead of the local store.
pub fn remote_path() -> Result<PathBuf> {
    Ok(home()?.join("remote.json"))
}

/// Create a new file readable only by the current user. Fails if it exists.
///
/// On Windows, files under the user profile inherit an ACL limited to the
/// user, SYSTEM and Administrators, which is the equivalent of 0600 there.
pub fn create_private(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path).map_err(|e| format!("{}: {e}", path.display()).into())
}

/// Write `contents` to `path` with owner-only permissions, replacing it.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    create_private(&tmp)?.write_all(contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Reject files that other users can read (Unix only).
fn check_private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(format!("{} is readable by others; run chmod 600", path.display()).into());
        }
    }
    let _ = path;
    Ok(())
}

/// Run a user-supplied shell command and return its stdout.
pub fn shell(cmd: &str) -> Result<String> {
    let out = if cfg!(windows) {
        Command::new("cmd").args(["/C", cmd]).output()?
    } else {
        Command::new("/bin/sh").args(["-c", cmd]).output()?
    };
    if !out.status.success() {
        return Err(format!("command failed: {cmd}").into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A 256-bit SQLCipher key, held as 64 lowercase hex chars.
pub struct Key(String);

impl Key {
    pub fn generate() -> Result<Self> {
        Ok(Key(hex(&random::<32>()?)))
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
    /// password manager) wins, otherwise the owner-only key file.
    pub fn load() -> Result<Self> {
        if let Ok(cmd) = std::env::var("RECALL_KEY_CMD") {
            return Key::parse(&shell(&cmd)?);
        }
        let path = key_path()?;
        if !path.exists() {
            return Err(format!("no key at {} — run `recall init`", path.display()).into());
        }
        check_private(&path)?;
        Key::parse(&fs::read_to_string(&path)?)
    }

    /// Write the key file with owner-only permissions. Refuses to overwrite.
    pub fn save(&self) -> Result<PathBuf> {
        let path = key_path()?;
        writeln!(create_private(&path)?, "{}", self.0)?;
        Ok(path)
    }
}

/// Create the home directory, owner-only on Unix.
pub fn ensure_home() -> Result<PathBuf> {
    let h = home()?;
    fs::create_dir_all(&h)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&h, fs::Permissions::from_mode(0o700))?;
    }
    Ok(h)
}

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut raw = [0u8; N];
    getrandom::fill(&mut raw).map_err(|e| format!("rng: {e}"))?;
    Ok(raw)
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
