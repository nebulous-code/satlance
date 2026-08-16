//! Money. Invariant 17: integer satoshis everywhere in logic and on the
//! wire; BTC formatting exists only for human-facing message text.

/// Convert a BTC amount as bitcoind serializes it (a JSON number with up to
/// 8 decimal places) into integer satoshis. f64 holds every valid bitcoind
/// amount exactly enough that rounding to the nearest satoshi is exact
/// (21e14 sat < 2^53), so `round()` removes any parse epsilon.
pub fn btc_to_sat(btc: f64) -> i64 {
    (btc * 100_000_000.0).round() as i64
}

/// Human-facing only (heartbeat/alert text). Never feed this back into logic.
pub fn format_btc(sat: i64) -> String {
    let neg = sat < 0;
    let a = sat.unsigned_abs();
    let whole = a / 100_000_000;
    let frac = a % 100_000_000;
    let mut s = format!("{whole}.{frac:08}");
    // trim trailing zeros but keep at least one decimal place
    while s.ends_with('0') && !s.ends_with(".0") {
        s.pop();
    }
    if neg {
        s.insert(0, '-');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_exactly() {
        for sat in [0i64, 1, 546, 100_000_000, 18_700_000, 2_100_000_000_000_000] {
            let btc: f64 = format!("{}", sat as f64 / 1e8).parse().unwrap();
            assert_eq!(btc_to_sat(btc), sat, "sat={sat}");
        }
        // classic float-trap value: 0.1 BTC
        assert_eq!(btc_to_sat(0.1), 10_000_000);
        assert_eq!(btc_to_sat(0.00000001), 1);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_btc(18_700_000), "0.187");
        assert_eq!(format_btc(0), "0.0");
        assert_eq!(format_btc(100_000_000), "1.0");
        assert_eq!(format_btc(-50_000_000), "-0.5");
    }
}
