//! Reconciliation (design §8.3): the confirmed/observed two-baseline model.
//!
//! Outpoint matching (detect.rs) is the theft detector; THIS module is the
//! backstop. `reconciled_*` is the user-blessed anchor (trust signal is
//! `reconciled_at`, never the raw number). `observed_*` is ephemeral —
//! promote-and-wipe on confirm. "Disputed" is never stored; it's a transient
//! rescan trigger that returns to awaiting.
//!
//! Backstop math simplification (documented in STATUS.md): instead of full
//! UTXO-set-vs-preauth-ledger arithmetic, the periodic check compares
//! observed vs reconciled and asks "is there an event row explaining the
//! delta since reconciled_at?" — no explaining record ⇒ the real-time
//! detector missed something ⇒ alarm.

use log::{info, warn};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::rpc::{RpcClient, RpcError};
use crate::sats::{btc_to_sat, format_btc};
use crate::{db, outbox, timeutil};

/// Trusted-balance read for a Core wallet: `getbalances` → mine.trusted,
/// converted to integer satoshis at the boundary (invariant 17).
pub fn compute_balance(rpc: &RpcClient, core_ref: &str) -> Result<i64, RpcError> {
    let v = rpc.get_balances(core_ref)?;
    let btc = v["mine"]["trusted"].as_f64().unwrap_or(0.0);
    Ok(btc_to_sat(btc))
}

/// Enqueue a "confirm this balance?" prompt (degraded tier — informational,
/// non-blocking per §8.3; unanswered prompts become ambient heartbeat state).
/// The payload carries `observed_at` so a stale confirm can't bless a number
/// that changed since the prompt (§11.1).
pub fn prompt_confirm(
    conn: &Connection,
    wallet_id: i64,
    label: &str,
    sat: i64,
    observed_at: i64,
    context: &str,
) {
    let mut p = outbox::payload(
        outbox::TIER_DEGRADED,
        "balance_confirm_prompt",
        &format!("Confirm balance for '{label}'"),
        &format!(
            "{context} Observed balance: {} BTC ({sat} sat). \
             Confirm with: satsurv balance confirm {wallet_id} --observed-at {observed_at}",
            format_btc(sat)
        ),
        Some(label),
        None,
        Some(sat),
        None,
    );
    p["wallet_id"] = json!(wallet_id);
    p["observed_at"] = json!(timeutil::iso8601(observed_at));
    p["observed_at_unix"] = json!(observed_at);
    if let Err(e) = outbox::enqueue(conn, outbox::TIER_DEGRADED, "balance_confirm_prompt", Some(wallet_id), None, &p) {
        warn!("enqueue balance prompt failed: {e}");
    }
}

/// A reconciliation checkpoint (§8.4): compute a fresh balance into
/// `observed_*` (scans=1, idle) and prompt. Triggered by pre-auth resolution
/// (consumed OR expired — both endings) via the `checkpoint_due:` meta flag.
pub fn checkpoint(conn: &Connection, rpc: &RpcClient, wallet_id: i64, context: &str) {
    let (label, core_ref): (String, String) = match conn.query_row(
        "SELECT label, core_wallet_ref FROM wallet WHERE id=?1",
        [wallet_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ) {
        Ok(t) => t,
        Err(_) => return,
    };
    match compute_balance(rpc, &core_ref) {
        Ok(sat) => {
            let now = timeutil::now();
            let _ = conn.execute(
                "UPDATE wallet SET observed_sat=?1, observed_at=?2, observed_scans=1,
                 observed_status='idle' WHERE id=?3",
                params![sat, now, wallet_id],
            );
            prompt_confirm(conn, wallet_id, &label, sat, now, context);
        }
        Err(e) => warn!("checkpoint balance compute failed for '{label}': {e}"),
    }
}

/// POST /wallets/{id}/balance/confirm — "yes". Stale-guarded: the caller
/// presents the `observed_at` it saw; mismatch ⇒ `stale_observation` (the
/// balance moved since the prompt — re-read before blessing).
/// Promote-and-wipe (§8.5); arms the wallet if it was `awaiting` (§5.4 gate).
pub fn confirm(conn: &Connection, wallet_id: i64, observed_at_unix: i64) -> Result<String, &'static str> {
    let row: Option<(String, String, Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT label, arming_state, observed_sat, observed_at FROM wallet WHERE id=?1",
            [wallet_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|_| "db_error")?;
    let (label, state, observed_sat, observed_at) = row.ok_or("wallet_not_found")?;
    let sat = observed_sat.ok_or("nothing_to_confirm")?;
    let at = observed_at.ok_or("nothing_to_confirm")?;
    if at != observed_at_unix {
        return Err("stale_observation");
    }

    // Multi-row state change in one transaction (invariant 18).
    conn.execute_batch("BEGIN IMMEDIATE").map_err(|_| "db_error")?;
    let res = (|| -> rusqlite::Result<String> {
        conn.execute(
            "UPDATE wallet SET reconciled_sat=?1, reconciled_at=?2,
             observed_sat=NULL, observed_at=NULL, observed_scans=0,
             observed_status='idle' WHERE id=?3",
            params![sat, at, wallet_id],
        )?;
        let new_state = if state == "awaiting" {
            conn.execute(
                "UPDATE wallet SET arming_state='armed' WHERE id=?1",
                [wallet_id],
            )?;
            info!("wallet '{label}': balance confirmed — ARMED");
            "armed".to_string()
        } else {
            info!("wallet '{label}': balance confirmed — baseline advanced");
            state
        };
        Ok(new_state)
    })();
    match res {
        Ok(s) => {
            conn.execute_batch("COMMIT").map_err(|_| "db_error")?;
            Ok(s)
        }
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err("db_error")
        }
    }
}

/// POST /wallets/{id}/balance/dispute — "no, keep scanning" (§8.3). Transient:
/// set scanning, bump gap limit (listdescriptors → doubled range, capped),
/// kick a rescan, recompute, return to awaiting. Never a resting state.
/// Synchronous on regtest-scale wallets; `observed_status='scanning'` covers
/// a heartbeat overlapping a slow mainnet rescan.
pub fn dispute(conn: &Connection, rpc: &RpcClient, wallet_id: i64) -> Result<(), String> {
    let (label, core_ref): (String, String) = conn
        .query_row(
            "SELECT label, core_wallet_ref FROM wallet WHERE id=?1",
            [wallet_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "wallet_not_found".to_string())?;

    conn.execute(
        "UPDATE wallet SET observed_status='scanning' WHERE id=?1",
        [wallet_id],
    )
    .map_err(|e| e.to_string())?;
    info!("wallet '{label}': balance disputed — bumping gap limit + rescanning");

    match bump_and_rescan(rpc, conn, &core_ref)? {
        BumpResult::Done => complete_rescan(conn, rpc, wallet_id),
        BumpResult::Started => {
            // The import call timed out: a deep (mainnet) rescan is running
            // inside the node. Stay 'scanning' — the heartbeat shows
            // "(rescan in progress)" and the worker's finish_pending_rescans
            // duty completes the dispute when getwalletinfo.scanning clears.
            info!("wallet '{label}': rescan running in the node — will recompute when it completes");
            Ok(())
        }
    }
}

/// Tail of a dispute: recompute the balance, advance observed_*, return to
/// idle, prompt. Called synchronously when the rescan finished inline
/// (regtest-fast), or from the worker duty once a slow node-side rescan ends.
pub fn complete_rescan(conn: &Connection, rpc: &RpcClient, wallet_id: i64) -> Result<(), String> {
    let (label, core_ref, prev_sat): (String, String, Option<i64>) = conn
        .query_row(
            "SELECT label, core_wallet_ref, observed_sat FROM wallet WHERE id=?1",
            [wallet_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| "wallet_not_found".to_string())?;
    let now = timeutil::now();
    match compute_balance(rpc, &core_ref) {
        Ok(sat) => {
            let found_more = prev_sat.map(|p| sat > p).unwrap_or(false);
            if found_more {
                // New UTXO found: bump observed, reset scans to 1 (§8.3).
                conn.execute(
                    "UPDATE wallet SET observed_sat=?1, observed_at=?2, observed_scans=1,
                     observed_status='idle' WHERE id=?3",
                    params![sat, now, wallet_id],
                )
                .map_err(|e| e.to_string())?;
            } else {
                conn.execute(
                    "UPDATE wallet SET observed_sat=?1, observed_at=?2,
                     observed_scans=observed_scans+1, observed_status='idle' WHERE id=?3",
                    params![sat, now, wallet_id],
                )
                .map_err(|e| e.to_string())?;
            }
            let note = if found_more {
                "Rescan found additional funds."
            } else {
                "Rescan complete — no further UTXOs found."
            };
            prompt_confirm(conn, wallet_id, &label, sat, now, note);
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute(
                "UPDATE wallet SET observed_status='idle' WHERE id=?1",
                [wallet_id],
            );
            Err(format!("rescan balance recompute failed: {e}"))
        }
    }
}

/// Worker duty: finish disputes whose node-side rescan outlived the RPC
/// timeout. For every wallet stuck in observed_status='scanning', check
/// whether the node is still scanning; when it's done, run the dispute tail.
pub fn finish_pending_rescans(conn: &Connection, rpc: &RpcClient) {
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT id, label, core_wallet_ref FROM wallet WHERE observed_status='scanning'",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|m| m.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };
    for (id, label, core_ref) in rows {
        let info = match rpc.get_wallet_info(&core_ref) {
            Ok(i) => i,
            Err(_) => continue, // node trouble: watchdog's job
        };
        if info["scanning"].is_object() {
            continue; // still rescanning — heartbeat shows "(rescan in progress)"
        }
        if let Err(e) = complete_rescan(conn, rpc, id) {
            warn!("wallet '{label}': finishing rescan failed: {e}");
        }
    }
}

/// Outcome of a gap-limit bump: the node either finished the re-import +
/// rescan inline, or took the job and is still scanning (RPC timed out).
enum BumpResult {
    Done,
    Started,
}

/// Gap-limit bump: read current descriptors, re-import each ranged descriptor
/// with a doubled range (start 1000 → cap 10000, §13), full-history rescan.
/// The descriptor strings pass straight from Core back to Core — never
/// persisted, never logged (invariant 6).
fn bump_and_rescan(rpc: &RpcClient, conn: &Connection, core_ref: &str) -> Result<BumpResult, String> {
    let cap = db::config_get_i64(conn, "gap_limit_cap");
    let descs = rpc
        .list_descriptors(core_ref)
        .map_err(|e| format!("listdescriptors failed: {e}"))?;
    let list = descs["descriptors"].as_array().cloned().unwrap_or_default();
    let mut requests = Vec::new();
    for d in &list {
        let desc = match d["desc"].as_str() {
            Some(s) => s,
            None => continue,
        };
        // Only ranged descriptors have a gap limit to bump.
        let range_end = d["range"]
            .as_array()
            .and_then(|r| r.get(1))
            .and_then(|v| v.as_i64());
        let new_end = match range_end {
            Some(end) => (end * 2).min(cap),
            None => continue,
        };
        if Some(new_end) == range_end {
            continue; // already at cap
        }
        requests.push(json!({
            "desc": desc,
            "range": [0, new_end],
            "timestamp": 0,          // full-history rescan
            "watchonly": true,
            "active": false,
        }));
    }
    if requests.is_empty() {
        info!("dispute rescan: no ranged descriptors below cap — recomputing only");
        return Ok(BumpResult::Done);
    }
    match rpc.import_descriptors(core_ref, json!(requests)) {
        Ok(_) => Ok(BumpResult::Done),
        // Short import timeout = the node took the job and is rescanning
        // (a mainnet full-history rescan runs for hours inside the call).
        Err(RpcError::Transport(_)) => Ok(BumpResult::Started),
        Err(e) => Err(format!("importdescriptors failed: {e}")),
    }
}

/// The backstop's core test, pure for unit testing: is an observed balance
/// drop bigger than every spend the real-time detector recorded since the
/// anchor? If so the detector missed something — that's the alarm case.
pub fn drop_is_unexplained(reconciled_sat: i64, observed_sat: i64, evented_spend_sat: i64) -> bool {
    observed_sat < reconciled_sat && (reconciled_sat - observed_sat) > evented_spend_sat
}

/// Periodic backstop (§8.4 heartbeat reconciliation), simplified per module
/// doc. Only meaningful for armed wallets with an established baseline
/// (`reconciled_at IS NULL` ⇒ never trust ⇒ skip; the arming gate handles it).
pub fn backstop_check(conn: &Connection, rpc: &RpcClient) {
    let wallets: Vec<(i64, String, String, i64, Option<i64>)> = {
        let mut stmt = match conn.prepare(
            "SELECT id, label, core_wallet_ref, reconciled_sat, reconciled_at
             FROM wallet WHERE arming_state='armed' AND reconciled_at IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        match stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        {
            Ok(v) => v,
            Err(_) => return,
        }
    };

    for (id, label, core_ref, reconciled_sat, reconciled_at) in wallets {
        let sat = match compute_balance(rpc, &core_ref) {
            Ok(s) => s,
            Err(_) => continue, // node trouble is the watchdog's job, not ours
        };
        if sat == reconciled_sat {
            continue; // consistent — the common case
        }
        let anchor = reconciled_at.unwrap_or(0);
        // "Explained" is arithmetic, not a row count: a drop can never
        // legitimately exceed the total value of the outpoints the detector
        // evented since the anchor. (A COUNT would let one small detected
        // spend mask an arbitrarily large undetected one.)
        let evented_sat: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(amount_sat),0) FROM event
                 WHERE wallet_id=?1 AND detected_at>=?2",
                params![id, anchor],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let now = timeutil::now();

        if drop_is_unexplained(reconciled_sat, sat, evented_sat) {
            // The dangerous case: balance dropped, real-time detector saw
            // nothing. Escalate (§8.4) — this is what the backstop exists for.
            warn!("BACKSTOP: wallet '{label}' balance below baseline with NO detected spend");
            let p = outbox::payload(
                outbox::TIER_ALARM,
                "backstop_discrepancy",
                &format!("BALANCE DISCREPANCY: '{label}'"),
                &format!(
                    "Balance is {} BTC but the confirmed baseline is {} BTC, and no spend \
                     was detected since the last reconciliation. The real-time detector may \
                     have missed a spend. Investigate immediately.",
                    format_btc(sat),
                    format_btc(reconciled_sat)
                ),
                Some(&label),
                None,
                Some(sat),
                None,
            );
            let _ = outbox::enqueue(conn, outbox::TIER_ALARM, "backstop_discrepancy", Some(id), None, &p);
            // Surface as an observation so the heartbeat carries it forward.
            let _ = conn.execute(
                "UPDATE wallet SET observed_sat=?1, observed_at=?2, observed_scans=1,
                 observed_status='idle' WHERE id=?3 AND observed_sat IS NULL",
                params![sat, now, id],
            );
        } else {
            // Explained drop (detected spend) or growth (deposit): surface as
            // an awaiting observation + prompt, only if nothing's already pending.
            let pending: Option<i64> = conn
                .query_row("SELECT observed_sat FROM wallet WHERE id=?1", [id], |r| r.get(0))
                .ok()
                .flatten();
            if pending.is_some() {
                continue; // already awaiting a confirm — don't spam
            }
            let _ = conn.execute(
                "UPDATE wallet SET observed_sat=?1, observed_at=?2, observed_scans=1,
                 observed_status='idle' WHERE id=?3",
                params![sat, now, id],
            );
            let why = if sat > reconciled_sat {
                "Balance increased (deposit?)."
            } else {
                "Balance changed (explained by detected spend)."
            };
            prompt_confirm(conn, id, &label, sat, now, why);
        }
    }
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

    fn add_wallet(c: &Connection, label: &str, state: &str) -> i64 {
        c.execute(
            "INSERT INTO wallet(label,core_wallet_ref,arming_state,added_at) VALUES(?1,?2,?3,0)",
            params![label, format!("satsurv-{label}"), state],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    #[test]
    fn confirm_promotes_and_wipes_and_arms() {
        let c = mem();
        let id = add_wallet(&c, "cold", "awaiting");
        c.execute(
            "UPDATE wallet SET observed_sat=50000000, observed_at=111, observed_scans=2 WHERE id=?1",
            [id],
        )
        .unwrap();
        let state = confirm(&c, id, 111).unwrap();
        assert_eq!(state, "armed");
        let (rs, ra, os, oa, scans): (i64, i64, Option<i64>, Option<i64>, i64) = c
            .query_row(
                "SELECT reconciled_sat,reconciled_at,observed_sat,observed_at,observed_scans FROM wallet WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!((rs, ra), (50_000_000, 111));
        assert!(os.is_none() && oa.is_none());
        assert_eq!(scans, 0);
    }

    #[test]
    fn confirm_rejects_stale_observation() {
        let c = mem();
        let id = add_wallet(&c, "cold", "awaiting");
        c.execute(
            "UPDATE wallet SET observed_sat=50000000, observed_at=222 WHERE id=?1",
            [id],
        )
        .unwrap();
        assert_eq!(confirm(&c, id, 111), Err("stale_observation"));
        // still awaiting — a stale yes blesses nothing
        let s: String = c
            .query_row("SELECT arming_state FROM wallet WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(s, "awaiting");
    }

    #[test]
    fn confirm_with_nothing_pending_errors() {
        let c = mem();
        let id = add_wallet(&c, "cold", "armed");
        assert_eq!(confirm(&c, id, 1), Err("nothing_to_confirm"));
    }

    #[test]
    fn backstop_explained_is_arithmetic_not_a_row_count() {
        // baseline 5.0, observed 4.99, evented spends total 0.01 -> explained
        assert!(!drop_is_unexplained(500_000_000, 499_000_000, 1_000_000));
        // baseline 5.0, observed 2.0, but only 0.01 of spends evented ->
        // the detector missed something: ALARM (a COUNT-based check would
        // have called this "explained" because one event row existed)
        assert!(drop_is_unexplained(500_000_000, 200_000_000, 1_000_000));
        // drop with zero evented spends -> alarm
        assert!(drop_is_unexplained(500_000_000, 499_999_999, 0));
        // growth is never an unexplained drop
        assert!(!drop_is_unexplained(500_000_000, 600_000_000, 0));
        // spent-outpoint totals exceed the net drop (change returned) -> explained
        assert!(!drop_is_unexplained(500_000_000, 480_000_000, 50_000_000));
    }

    #[test]
    fn confirm_outside_awaiting_keeps_state() {
        // A checkpoint confirm on an armed wallet advances the baseline
        // without touching the arming state.
        let c = mem();
        let id = add_wallet(&c, "cold", "armed");
        c.execute(
            "UPDATE wallet SET observed_sat=10, observed_at=5 WHERE id=?1",
            [id],
        )
        .unwrap();
        assert_eq!(confirm(&c, id, 5).unwrap(), "armed");
    }
}
