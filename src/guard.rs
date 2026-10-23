//! Write-side guards: normalisation and secret detection.
//!
//! Hand-rolled scanners instead of a regex engine keep the binary small and
//! the hot path allocation-light. Each check is deliberately high-precision:
//! a false reject is cheap (the agent rephrases), a stored secret is not.

pub const MAX_CHARS: usize = 500;

/// Collapse whitespace, drop control characters, trim.
pub fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else if !c.is_control() && !is_invisible(c) {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Zero-width and bidi-override characters used to hide text from humans.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}

/// Returns a human-readable reason if `s` looks like it contains a secret or
/// government/financial identifier.
pub fn secret_reason(s: &str) -> Option<&'static str> {
    const PREFIXES: &[(&str, usize, &str)] = &[
        ("AKIA", 16, "an AWS access key"),
        ("ASIA", 16, "an AWS access key"),
        ("sk-", 20, "an API key"),
        ("sk_live_", 16, "a Stripe key"),
        ("rk_live_", 16, "a Stripe key"),
        ("ghp_", 30, "a GitHub token"),
        ("gho_", 30, "a GitHub token"),
        ("ghs_", 30, "a GitHub token"),
        ("github_pat_", 30, "a GitHub token"),
        ("glpat-", 20, "a GitLab token"),
        ("xoxb-", 20, "a Slack token"),
        ("xoxp-", 20, "a Slack token"),
        ("AIza", 35, "a Google API key"),
        ("hf_", 30, "a Hugging Face token"),
        ("npm_", 30, "an npm token"),
    ];
    if s.contains("PRIVATE KEY") && s.contains("-----BEGIN") {
        return Some("a private key");
    }
    for word in s.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | ',' | ';')) {
        let w = word.trim_end_matches(['.', ':']);
        for (p, min, what) in PREFIXES {
            if let Some(rest) = w.strip_prefix(p)
                && rest.len() >= *min
                && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Some(what);
            }
        }
        if is_jwt(w) {
            return Some("a JWT");
        }
        if looks_random(w) {
            return Some("a high-entropy token");
        }
    }
    if has_card_number(s) {
        return Some("a payment card number");
    }
    if has_ssn(s) {
        return Some("a social security number");
    }
    if has_credential_assignment(s) {
        return Some("a password or credential");
    }
    None
}

fn is_jwt(w: &str) -> bool {
    let parts: Vec<&str> = w.split('.').collect();
    parts.len() == 3
        && w.starts_with("eyJ")
        && parts.iter().all(|p| p.len() >= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

/// Long token mixing character classes with high Shannon entropy.
fn looks_random(w: &str) -> bool {
    if w.len() < 32 || w.contains("://") {
        return false;
    }
    if !w.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b)) {
        return false;
    }
    let (mut lower, mut upper, mut digit) = (false, false, false);
    let mut counts = [0u32; 256];
    for b in w.bytes() {
        lower |= b.is_ascii_lowercase();
        upper |= b.is_ascii_uppercase();
        digit |= b.is_ascii_digit();
        counts[b as usize] += 1;
    }
    if (lower as u8 + upper as u8 + digit as u8) < 2 {
        return false;
    }
    let n = w.len() as f64;
    let entropy: f64 = counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum();
    entropy > 4.0
}

/// 13–19 digits (spaces/dashes allowed between) passing the Luhn check.
fn has_card_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && b[i - 1].is_ascii_alphanumeric()) {
            i += 1;
            continue;
        }
        let mut digits = Vec::with_capacity(19);
        let mut j = i;
        while j < b.len() && digits.len() < 19 {
            if b[j].is_ascii_digit() {
                digits.push(b[j] - b'0');
            } else if !(matches!(b[j], b' ' | b'-') && j + 1 < b.len() && b[j + 1].is_ascii_digit()) {
                break;
            }
            j += 1;
        }
        if digits.len() >= 13 && luhn(&digits) {
            return true;
        }
        i = j.max(i + 1);
    }
    false
}

fn luhn(d: &[u8]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            let x = x as u32;
            if i % 2 == 1 { if x * 2 > 9 { x * 2 - 9 } else { x * 2 } } else { x }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// US SSN shape: ddd-dd-dddd not embedded in a longer number.
fn has_ssn(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(11).enumerate().any(|(i, w)| {
        let shape = w.iter().enumerate().all(|(k, &c)| if k == 3 || k == 6 { c == b'-' } else { c.is_ascii_digit() });
        let before = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let after = i + 11 == b.len() || !b[i + 11].is_ascii_alphanumeric();
        shape && before && after
    })
}

/// "password: hunter2", "api key = abc…", "my pin is 1234".
fn has_credential_assignment(s: &str) -> bool {
    const WORDS: &[&str] = &["password", "passwd", "passcode", "pwd", "api key", "api_key", "apikey", "secret key", "access token", "pin code", "pin is", "pin:"];
    let lower = s.to_lowercase();
    for w in WORDS {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(w) {
            let after = lower[from + pos + w.len()..].trim_start();
            let value = after
                .strip_prefix(':')
                .or_else(|| after.strip_prefix('='))
                .or_else(|| after.strip_prefix("is "))
                .map(str::trim_start)
                .or(if w.ends_with("is") || w.ends_with(':') { Some(after) } else { None });
            if let Some(v) = value {
                let tok: String = v.chars().take_while(|c| !c.is_whitespace()).collect();
                let tok = tok.trim_end_matches(['.', ',', ';']);
                // A value, not prose like "password is managed by 1Password".
                if !tok.is_empty() && !STOP.contains(&tok) {
                    return true;
                }
            }
            from += pos + w.len();
        }
    }
    false
}

const STOP: &[&str] = &["a", "an", "the", "managed", "stored", "kept", "in", "saved", "required", "not", "never", "always", "set", "rotated", "changed"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises() {
        assert_eq!(normalise("  a\n\tb\u{200B}c \u{7}d  "), "a bc d");
    }

    #[test]
    fn rejects_secrets() {
        for s in [
            "my key is AKIAIOSFODNN7EXAMPLE",
            "token sk-proj-abcdefghijklmnopqrstuvwxyz123456",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "-----BEGIN OPENSSH PRIVATE KEY----- abc",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "card 4111 1111 1111 1111",
            "SSN 123-45-6789",
            "wifi password: hunter2",
            "Their api key = abc123",
            "blob Zx9Qm2LkP8vR4tYw7NbC3hJ6gF1dS5aE0uIoXyZq",
        ] {
            assert!(secret_reason(s).is_some(), "missed: {s}");
        }
    }

    #[test]
    fn allows_ordinary_facts() {
        for s in [
            "User is vegetarian and allergic to peanuts.",
            "User's passwords are managed in 1Password.",
            "User was born in 1988 and lives at 221B Baker Street.",
            "User's phone is an iPhone 17 Pro.",
            "User works at Acme Corp on the payments team since 2023-04-01.",
            "User likes https://github.com/rust-lang/rust-analyzer/blob/master/README.md",
            "User prefers sk-style keyboards? no, prefers split keyboards.",
        ] {
            assert_eq!(secret_reason(s), None, "false positive: {s}");
        }
    }
}
