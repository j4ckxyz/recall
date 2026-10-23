//! Memory dates: when a fact was learned or became true.
//!
//! Stored as sortable text at whatever precision the agent knew:
//! `2019`, `2019-08`, `2019-08-09` or `2019-08-09 14:05` (UTC).

use crate::Result;

/// Current time as `YYYY-MM-DD HH:MM` UTC.
pub fn now() -> String {
    format_unix(crate::store::now())
}

pub fn format_unix(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// Accepts `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, and `YYYY-MM-DD[T ]HH:MM[:SS][Z|±HH:MM]`.
/// Offsets are converted to UTC. Future dates are rejected.
pub fn parse(input: &str) -> Result<String> {
    let s = input.trim();
    let bad = || format!("invalid date '{input}'; use YYYY-MM-DD or YYYY-MM-DDTHH:MM");
    let num = |t: &str, lo: i64, hi: i64| -> std::result::Result<i64, String> {
        let v: i64 = t.parse().map_err(|_| bad())?;
        if t.bytes().all(|b| b.is_ascii_digit()) && (lo..=hi).contains(&v) { Ok(v) } else { Err(bad()) }
    };
    let (date, time) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let parts: Vec<&str> = date.split('-').collect();
    if parts[0].len() != 4 || parts.iter().skip(1).any(|p| p.len() != 2) || parts.len() > 3 {
        return Err(bad().into());
    }
    let y = num(parts[0], 1900, 2999)?;
    let m = parts.get(1).map(|p| num(p, 1, 12)).transpose()?;
    let d = parts.get(2).map(|p| num(p, 1, days_in_month(y, m.unwrap_or(1)))).transpose()?;

    let out = match (m, d, time) {
        (None, _, None) => format!("{y:04}"),
        (Some(m), None, None) => format!("{y:04}-{m:02}"),
        (Some(m), Some(d), None) => format!("{y:04}-{m:02}-{d:02}"),
        (Some(m), Some(d), Some(t)) => {
            let (clock, offset) = split_offset(t).ok_or_else(bad)?;
            let hm: Vec<&str> = clock.split(':').collect();
            if !(2..=3).contains(&hm.len()) {
                return Err(bad().into());
            }
            let (h, mi) = (num(hm[0], 0, 23)?, num(hm[1], 0, 59)?);
            if let Some(sec) = hm.get(2) {
                num(&sec[..sec.find('.').unwrap_or(sec.len())], 0, 60)?;
            }
            let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 - offset * 60;
            format_unix(secs)
        }
        _ => return Err(bad().into()),
    };
    if out.as_str() > now().as_str() && !now().starts_with(&out) {
        return Err(format!("date '{input}' is in the future").into());
    }
    Ok(out)
}

/// Split `HH:MM[:SS][Z|±HH:MM]` into the clock part and an offset in minutes.
fn split_offset(t: &str) -> Option<(&str, i64)> {
    if let Some(c) = t.strip_suffix('Z') {
        return Some((c, 0));
    }
    match t.rfind(['+', '-']) {
        Some(i) => {
            let (c, o) = (&t[..i], &t[i..]);
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let o = o[1..].replace(':', "");
            if o.len() != 4 || !o.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mins = o[..2].parse::<i64>().ok()? * 60 + o[2..].parse::<i64>().ok()?;
            Some((c, sign * mins))
        }
        None => Some((t, 0)),
    }
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's civil calendar algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_precisions() {
        assert_eq!(parse("2019").unwrap(), "2019");
        assert_eq!(parse("2019-08").unwrap(), "2019-08");
        assert_eq!(parse("2019-08-09").unwrap(), "2019-08-09");
        assert_eq!(parse("2019-08-09T14:05").unwrap(), "2019-08-09 14:05");
        assert_eq!(parse("2019-08-09T14:05:33Z").unwrap(), "2019-08-09 14:05");
        assert_eq!(parse("2019-08-09T01:30+02:00").unwrap(), "2019-08-08 23:30");
        assert_eq!(parse("2024-02-29").unwrap(), "2024-02-29");
    }

    #[test]
    fn rejects_bad() {
        for s in ["yesterday", "2019-13-01", "2023-02-29", "19-08-09", "2999-01-01", "2019-08-09T25:00"] {
            assert!(parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn formats_unix() {
        assert_eq!(format_unix(0), "1970-01-01 00:00");
        assert_eq!(format_unix(1_700_000_000), "2023-11-14 22:13");
    }
}
