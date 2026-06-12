//! Wallet arming state machine (design §5.3, transition table §8.6).
//!
//! Invariants 7-9: `armed` never lies; arming requires explicit balance
//! confirmation (never auto-arm); not-armed is loud.

use log::{info, warn};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::rpc::{RpcClient, RpcError};
use crate::sats::format_btc;
use crate::timeutil;
use crate::{db, outbox, reconcile};

pub fn set_state(conn: &Connection, wallet_id: i64, state: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE wallet SET arming_state=?1 WHERE id=?2",
        params![state, wallet_id],
    )?;
    Ok(())
}

pub fn get_state(conn: &Connection, wallet_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT arming_state FROM wallet WHERE id=?1",
        [wallet_id],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// POST /wallets — row created in `registered`. If a descriptor came with
/// the request (automated-import path) the caller forwards it straight to
/// `begin_import`; it is never persisted here (invariants 5, 6).
pub fn register(conn: &Connection, label: &str, core_ref: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO wallet(label,core_wallet_ref,arming_state,added_at)
         VALUES(?1,?2,'registered',?3)",
        params![label, core_ref, timeutil::now()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Daemon-automated import path (§5.3): create the keyless descriptor wallet
/// in Core if needed and `importdescriptors` both branches. The descriptor
/// transits this function and is never stored or logged.
pub fn begin_import(
    conn: &Connection,
    rpc: &RpcClient,
    wallet_id: i64,
    descriptors: &[String],
) -> Result<(), String> {
    let (label, core_ref): (String, String) = conn
        .query_row(
            "SELECT label,core_wallet_ref FROM wallet WHERE id=?1",
            [wallet_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "wallet_not_found".to_string())?;

    // ensure the Core wallet exists / is loaded
    let loaded = rpc.list_wallets().map_err(|e| e.to_string())?;
    if !loaded.contains(&core_ref) {
        match rpc.load_wallet(&core_ref) {
            Ok(_) => {}
            Err(RpcError::Node { code: -18, .. }) => {
                rpc.create_watchonly_wallet(&core_ref)
                    .map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
    }

    // Expand a single receive-branch descriptor to receive+change (§5.3 —
    // under-importing the change branch is the classic silent failure).
    let mut expanded: Vec<String> = Vec::new();
    for d in descriptors {
        expanded.push(d.clone());
        if d.contains("/0/*") && !descriptors.iter().any(|x| x.contains("/1/*")) {
            expanded.push(d.replace("/0/*", "/1/*"));
        }
    }

    let range = db::config_get_i64(conn, "gap_limit_start");
    let mut requests = Vec::new();
    for d in &expanded {
        // normalize checksum via the node; never log the descriptor itself
        let info = rpc
            .call(None, "getdescriptorinfo", json!([d]))
            .map_err(|_| "descriptor rejected by node (checksum/parse)".to_string())?;
        let canonical = info["descriptor"].as_str().unwrap_or(d).to_string();
        requests.push(json!({
            "desc": canonical,
            "timestamp": 0,          // full history rescan — cold storage may be old
            "range": [0, range],
            "active": false,
            "internal": d.contains("/1/*"),
            "watchonly": true,
            "label": ""
        }));
    }

    set_state(conn, wallet_id, "importing").map_err(|e| e.to_string())?;
    info!("wallet '{label}': descriptor import started (rescan running — NOT PROTECTED)");

    match rpc.import_descriptors(&core_ref, Value::Array(requests)) {
        Ok(res) => {
            let all_ok = res
                .as_array()
                .map(|a| a.iter().all(|r| r["success"].as_bool().unwrap_or(false)))
                .unwrap_or(false);
            if !all_ok {
                set_state(conn, wallet_id, "error").map_err(|e| e.to_string())?;
                notify_error(conn, wallet_id, &label, "descriptor import reported failure");
                return Err("import_failed".into());
            }
            Ok(())
        }
        Err(e) => {
            // EDGE 1: import/rescan failure -> error, NOT PROTECTED, degraded notice
            set_state(conn, wallet_id, "error").map_err(|er| er.to_string())?;
            notify_error(conn, wallet_id, &label, "descriptor import failed");
            Err(e.to_string())
        }
    }
}

fn notify_error(conn: &Connection, wallet_id: i64, label: &str, what: &str) {
    let p = outbox::payload(
        outbox::TIER_DEGRADED,
        "wallet_error",
        &format!("⚠ Wallet '{label}' — NOT PROTECTED"),
        &format!("{what}. The wallet is in 'error' state and is NOT being watched. \
                  Retry with the import/rescan command."),
        Some(label),
        None,
        None,
        None,
    );
    let _ = outbox::enqueue(conn, outbox::TIER_DEGRADED, "wallet_error", Some(wallet_id), None, &p);
}

/// Poll-driven progress for `registered` and `importing` wallets — runs every
/// worker tick. Detects the by-hand import path appearing in Core, watches
/// `getwalletinfo.scanning`, and on completion computes the first observed
/// balance and prompts (§5.4: the arming gate; never auto-arm).
pub fn poll_onboarding(conn: &Connection, rpc: &RpcClient) {
    let rows: Vec<(i64, String, String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT id,label,core_wallet_ref,arming_state FROM wallet
             WHERE arming_state IN ('registered','importing')",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        match stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .and_then(|m| m.collect())
        {
            Ok(v) => v,
            Err(_) => return,
        }
    };

    for (id, label, core_ref, state) in rows {
        // is the Core wallet visible yet?
        let loaded = match rpc.list_wallets() {
            Ok(l) => l,
            Err(_) => return, // node trouble; the watchdog handles it
        };
        if !loaded.contains(&core_ref) {
            if rpc.load_wallet(&core_ref).is_err() {
                continue; // by-hand path: still waiting for the user
            }
        }
        let info = match rpc.get_wallet_info(&core_ref) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let scanning = info["scanning"].is_object(); // false when idle, object while scanning

        if state == "registered" {
            // by-hand import detected: wallet exists in Core now
            let _ = set_state(conn, id, "importing");
            info!("wallet '{label}': detected in Core (by-hand path) — importing/rescan");
        }
        if scanning {
            continue; // still BLIND — stays importing (NOT PROTECTED)
        }
        // rescan complete -> awaiting + first balance prompt (§5.4)
        match reconcile::compute_balance(rpc, &core_ref) {
            Ok(sat) => {
                let now = timeutil::now();
                let _ = conn.execute(
                    "UPDATE wallet SET arming_state='awaiting', observed_sat=?1,
                     observed_at=?2, observed_scans=1, observed_status='idle' WHERE id=?3",
                    params![sat, now, id],
                );
                info!("wallet '{label}': scan complete — awaiting balance confirmation (NOT PROTECTED until confirmed)");
                reconcile::prompt_confirm(conn, id, &label, sat, now,
                    "Initial scan complete. Confirming arms the wallet.");
            }
            Err(e) => warn!("wallet '{label}': balance compute failed: {e}"),
        }
    }
}

/// POST /wallets/{id}/disarm — deliberate pause (distinct from blinded).
pub fn disarm(conn: &Connection, wallet_id: i64) -> Result<(), &'static str> {
    match get_state(conn, wallet_id).as_deref() {
        Some("armed") | Some("blinded") => {
            set_state(conn, wallet_id, "disarmed").map_err(|_| "db_error")?;
            Ok(())
        }
        Some(_) => Err("wallet_not_armed"),
        None => Err("wallet_not_found"),
    }
}

/// POST /wallets/{id}/arm — resume from `disarmed`. EDGE 2: a catch-up
/// reconciliation runs first; if the balance moved while paused, the wallet
/// goes to `awaiting` (confirm the delta) rather than silently `armed`.
pub fn arm(conn: &Connection, rpc: &RpcClient, wallet_id: i64) -> Result<String, String> {
    let (label, core_ref, state, reconciled_sat, reconciled_at): (String, String, String, i64, Option<i64>) =
        conn.query_row(
            "SELECT label,core_wallet_ref,arming_state,reconciled_sat,reconciled_at
             FROM wallet WHERE id=?1",
            [wallet_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(|_| "wallet_not_found".to_string())?;
    if state != "disarmed" {
        return Err("wallet_not_disarmed".into());
    }
    let sat = reconcile::compute_balance(rpc, &core_ref).map_err(|e| e.to_string())?;
    if reconciled_at.is_some() && sat == reconciled_sat {
        set_state(conn, wallet_id, "armed").map_err(|e| e.to_string())?;
        info!("wallet '{label}': re-armed (balance unchanged while paused)");
        Ok("armed".into())
    } else {
        let now = timeutil::now();
        conn.execute(
            "UPDATE wallet SET arming_state='awaiting', observed_sat=?1, observed_at=?2,
             observed_scans=1, observed_status='idle' WHERE id=?3",
            params![sat, now, wallet_id],
        )
        .map_err(|e| e.to_string())?;
        reconcile::prompt_confirm(conn, wallet_id, &label, sat, now,
            "Balance changed while the wallet was disarmed. Confirm to re-arm.");
        Ok("awaiting".into())
    }
}

/// Node lost: every `armed` wallet flips to `blinded` (invariant 7 — armed
/// never lies). Daemon-managed, involuntary, visually distinct from disarmed.
pub fn blind_all_armed(conn: &Connection) -> usize {
    conn.execute(
        "UPDATE wallet SET arming_state='blinded' WHERE arming_state='armed'",
        [],
    )
    .unwrap_or(0)
}

/// Node back: `blinded` wallets auto-recover to `armed` — the caller runs the
/// catch-up reconciliation (a detection pass) first.
pub fn unblind_all(conn: &Connection) -> usize {
    conn.execute(
        "UPDATE wallet SET arming_state='armed' WHERE arming_state='blinded'",
        [],
    )
    .unwrap_or(0)
}

/// Loud not-protected lines for status/heartbeat (§5.5): nags, never hides.
pub fn status_lines(conn: &Connection) -> (Vec<String>, Vec<String>) {
    let mut armed = Vec::new();
    let mut not_protected = Vec::new();
    let mut stmt = match conn.prepare(
        "SELECT label,arming_state,reconciled_sat,reconciled_at,observed_sat,
                observed_scans,observed_status FROM wallet ORDER BY label",
    ) {
        Ok(s) => s,
        Err(_) => return (armed, not_protected),
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
        ))
    });
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            let (label, state, rec_sat, rec_at, obs_sat, obs_scans, obs_status) = row;
            // §8.3 heartbeat notation: confirmed anchor + unconfirmed observation
            let balance = if rec_at.is_none() {
                "never reconciled — balance not trusted".to_string()
            } else if obs_status == "scanning" {
                format!("{} BTC reconciled (rescan in progress)", format_btc(rec_sat))
            } else if let Some(o) = obs_sat {
                format!(
                    "{} BTC reconciled ({} BTC observed — scanned {obs_scans}×, awaiting your confirmation)",
                    format_btc(rec_sat),
                    format_btc(o)
                )
            } else {
                format!("{} BTC reconciled", format_btc(rec_sat))
            };
            match state.as_str() {
                "armed" => armed.push(format!("{label}   {balance}   armed")),
                "blinded" => not_protected.push(format!(
                    "⛔ {label}   BLINDED — node unreachable (involuntary) — NOT PROTECTED"
                )),
                "disarmed" => not_protected.push(format!(
                    "⏸ {label}   disarmed by you — NOT PROTECTED"
                )),
                "importing" => not_protected.push(format!(
                    "⚠ {label}   importing (rescan running) — NOT PROTECTED"
                )),
                "awaiting" => not_protected.push(format!(
                    "⚠ {label}   awaiting your balance confirmation ({balance}) — NOT PROTECTED"
                )),
                "registered" => not_protected.push(format!(
                    "⚠ {label}   registered, descriptor not yet imported — NOT PROTECTED"
                )),
                _ => not_protected.push(format!(
                    "⚠ {label}   {state} — NOT PROTECTED"
                )),
            }
        }
    }
    (armed, not_protected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        db::init_schema(&c).unwrap();
        db::config_seed_defaults(&c).unwrap();
        c
    }

    #[test]
    fn blinded_is_distinct_from_disarmed_and_loud() {
        let c = mem();
        let id = register(&c, "vault", "vault-ref").unwrap();
        set_state(&c, id, "armed").unwrap();
        c.execute("UPDATE wallet SET reconciled_at=1, reconciled_sat=100", []).unwrap();

        assert_eq!(blind_all_armed(&c), 1);
        let (_armed, np) = status_lines(&c);
        assert!(np[0].contains("BLINDED"), "{np:?}");
        assert!(np[0].contains("NOT PROTECTED"));

        assert_eq!(unblind_all(&c), 1);
        let (armed, np) = status_lines(&c);
        assert_eq!(np.len(), 0);
        assert!(armed[0].contains("armed"));

        // disarm path is user-intent, different wording
        disarm(&c, id).unwrap();
        let (_a, np) = status_lines(&c);
        assert!(np[0].contains("disarmed by you"));
    }

    #[test]
    fn disarm_rejects_wrong_states() {
        let c = mem();
        let id = register(&c, "w", "w").unwrap();
        assert_eq!(disarm(&c, id), Err("wallet_not_armed"));
        assert_eq!(disarm(&c, 999), Err("wallet_not_found"));
    }

    #[test]
    fn never_reconciled_balance_is_not_trusted() {
        let c = mem();
        let id = register(&c, "w", "w").unwrap();
        set_state(&c, id, "armed").unwrap(); // contrived; reconciled_at still NULL
        let (armed, _np) = status_lines(&c);
        assert!(
            armed[0].contains("not trusted"),
            "reconciled_at IS NULL must read as untrusted (invariant 10): {armed:?}"
        );
    }

    #[test]
    fn status_never_shows_unwatched_wallet_as_armed() {
        let c = mem();
        for (label, state) in [
            ("a", "registered"), ("b", "importing"), ("c", "awaiting"),
            ("d", "blinded"), ("e", "error"), ("f", "disarmed"),
        ] {
            let id = register(&c, label, label).unwrap();
            set_state(&c, id, state).unwrap();
        }
        let (armed, np) = status_lines(&c);
        assert_eq!(armed.len(), 0);
        assert_eq!(np.len(), 6);
        assert!(np.iter().all(|l| l.contains("NOT PROTECTED")));
    }
}
