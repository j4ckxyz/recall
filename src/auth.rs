//! Web UI sign-in: PBKDF2-HMAC-SHA256 password hashes and session tokens.

use sha2::{Digest, Sha256};

use crate::Result;
use crate::config::{hex, random};

/// OWASP's 2023 recommendation for PBKDF2-HMAC-SHA256.
pub const ITERATIONS: u32 = 600_000;

pub const SESSION_SECS: i64 = 30 * 24 * 3600;

/// A random password: 4 groups of 5 unambiguous characters (~99 bits).
pub fn generate_password() -> Result<String> {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let raw = random::<20>()?;
    let chars: Vec<char> = raw.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    Ok(chars.chunks(5).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-"))
}

/// `pbkdf2-sha256$iterations$salt$hash`, all hex.
pub fn hash_password(password: &str, iterations: u32) -> Result<String> {
    let salt = random::<16>()?;
    let dk = pbkdf2(password.as_bytes(), &salt, iterations);
    Ok(format!("pbkdf2-sha256${iterations}${}${}", hex(&salt), hex(&dk)))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let [scheme, iters, salt, hash] = parts[..] else { return false };
    let (Ok(iters), Some(salt), Some(hash)) = (iters.parse::<u32>(), unhex(salt), unhex(hash)) else { return false };
    scheme == "pbkdf2-sha256" && constant_eq(&pbkdf2(password.as_bytes(), &salt, iters), &hash)
}

/// A new session token (sent as a cookie) and the hash we store for it.
pub fn new_session() -> Result<(String, [u8; 32])> {
    let token = hex(&random::<32>()?);
    let hash = session_hash(&token);
    Ok((token, hash))
}

pub fn session_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    // HMAC with the key's inner/outer pads absorbed once and cloned per round.
    let mut key = [0u8; 64];
    if password.len() > 64 {
        key[..32].copy_from_slice(&Sha256::digest(password));
    } else {
        key[..password.len()].copy_from_slice(password);
    }
    let pad = |byte: u8| {
        let mut h = Sha256::new();
        h.update(key.map(|k| k ^ byte));
        h
    };
    let (inner, outer) = (pad(0x36), pad(0x5c));
    let mac = |msg: &[&[u8]]| -> [u8; 32] {
        let mut i = inner.clone();
        msg.iter().for_each(|m| i.update(m));
        let mut o = outer.clone();
        o.update(i.finalize());
        o.finalize().into()
    };
    let mut u = mac(&[salt, &1u32.to_be_bytes()]);
    let mut out = u;
    for _ in 1..iterations {
        u = mac(&[&u]);
        out.iter_mut().zip(u).for_each(|(o, x)| *o ^= x);
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_rfc7914_vector() {
        // RFC 7914 §11: PBKDF2-HMAC-SHA256("passwd", "salt", 1, 64) — first 32 bytes.
        assert_eq!(hex(&pbkdf2(b"passwd", b"salt", 1)), "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc");
    }

    #[test]
    fn hashes_and_verifies() {
        let h = hash_password("correct horse", 1_000).unwrap();
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong horse", &h));
        assert!(!verify_password("correct horse", "garbage"));
        let p = generate_password().unwrap();
        assert_eq!((p.len(), p.matches('-').count()), (23, 3));
    }
}
