//! Time handling. Design §11.2: "Unix in the DB, ISO on the wire."
//! Integer unix seconds everywhere internally; ISO-8601 UTC strings only at
//! the API boundary. Civil-date math uses Howard Hinnant's days algorithm to
//! avoid pulling a date crate.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// days since 1970-01-01 -> (year, month, day). Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// unix seconds -> ISO-8601 UTC string, e.g. "2026-06-02T14:30:00Z".
pub fn iso8601(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// (year, month, day) -> days since 1970-01-01. Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Parse "YYYY-MM-DDTHH:MM:SSZ" (the only form we emit) -> unix seconds.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let (y, mo, d): (i64, u32, u32) = (
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
    );
    let mut tp = time.split(':');
    let (h, mi, sec): (i64, i64, i64) = (
        tp.next()?.parse().ok()?,
        tp.next()?.parse().ok()?,
        tp.next().unwrap_or("0").parse().ok()?,
    );
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn parse_roundtrip() {
        for t in [0i64, 1_780_410_600, 1_709_164_800, 1_735_689_599] {
            assert_eq!(parse_iso8601(&iso8601(t)), Some(t));
        }
        assert_eq!(parse_iso8601("not a date"), None);
    }

    #[test]
    fn known_instants() {
        // 2026-06-02T14:30:00Z
        assert_eq!(iso8601(1_780_410_600), "2026-06-02T14:30:00Z");
        // leap day
        assert_eq!(iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
        // end-of-year boundary
        assert_eq!(iso8601(1_735_689_599), "2024-12-31T23:59:59Z");
        assert_eq!(iso8601(1_735_689_600), "2025-01-01T00:00:00Z");
    }
}
