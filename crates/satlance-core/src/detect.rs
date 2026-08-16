//! Detection (design §6). The premise of the whole tool.
//!
//! Pull-shaped: ZMQ is only a trigger (invariant 4); every detection pass
//! here is RPC-driven and authoritative, so the same pass serves live ZMQ
//! triggers, periodic polling fallback, sequence-gap reconciliation, and
//! restart catch-up. Core/Knots resolves outpoint ownership (invariant 5);
//! the daemon keeps no UTXO mirror.
//!
//! Detection is by outpoint identity, never by amount (invariants 1-3).

use log::{info, warn};
use rusqlite::{params, Connection, OptionalExtension};

use crate::sats::{btc_to_sat, format_btc};
use crate::timeutil;
use crate::{auth, db, outbox, rpc::RpcClient};

pub struct SpentOutpoint {
    pub txid: String,
    pub vout: i64,
    pub amount_sat: i64,
}

pub struct WalletRow {
    pub id: i64,
    pub label: String,
    pub core_ref: String,
}

pub fn watched_wallets(conn: &Connection) -> rusqlite::Result<Vec<WalletRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,label,core_wallet_ref FROM wallet
         WHERE arming_state IN ('armed','blinded')",
    )?;
    let v = stmt
        .query_map([], |r| {
            Ok(WalletRow {
                id: r.get(0)?,
                label: r.get(1)?,
                core_ref: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(v)
}

/// One full detection pass: for every watched wallet, ask Core what happened
/// since our persisted position, resolve candidate spends to outpoints, and
/// record them. Advances the persisted position only if every wallet was
/// processed cleanly (a partial failure re-scans next pass; the dedup index
/// makes that idempotent — §8.4).
pub fn detection_pass(conn: &Connection, rpc: &RpcClient) -> Result<(), String> {
    let last_hash = db::meta_get(conn, "last_block_hash");
    // Capture the tip BEFORE scanning: a block mined mid-pass must stay
    // above the persisted floor so the next pass still covers it. (Persisting
    // a post-scan tip would skip anything mined between scan and persist.)
    let pre_scan_tip = rpc.get_best_block_hash().ok();
    let wallets = watched_wallets(conn).map_err(|e| e.to_string())?;
    let mut all_ok = true;

    for w in &wallets {
        if let Err(e) = scan_wallet(conn, rpc, w, last_hash.as_deref()) {
            warn!("detection pass: wallet '{}': {e}", w.label);
            all_ok = false;
        }
    }

    update_confirmations(conn, rpc);

    if all_ok {
        if let Some(tip) = pre_scan_tip {
            db::meta_set(conn, "last_block_hash", &tip).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn scan_wallet(
    conn: &Connection,
    rpc: &RpcClient,
    w: &WalletRow,
    last_hash: Option<&str>,
) -> Result<(), String> {
    let lsb = rpc
        .list_since_block(&w.core_ref, last_hash)
        .map_err(|e| e.to_string())?;
    let txs = lsb["transactions"].as_array().cloned().unwrap_or_default();

    // candidate spends: any tx where this wallet sent (category "send").
    // listsinceblock always includes unconfirmed txs, so mempool spends
    // surface here too (the 0-conf 'seen' path).
    let mut candidates: Vec<String> = txs
        .iter()
        .filter(|t| t["category"].as_str() == Some("send"))
        .filter_map(|t| t["txid"].as_str().map(String::from))
        .collect();
    candidates.sort();
    candidates.dedup();

    for txid in candidates {
        if let Err(e) = process_candidate_spend(conn, rpc, w, &txid) {
            return Err(format!("candidate {txid}: {e}"));
        }
    }
    Ok(())
}

/// Resolve which of OUR outpoints a spending tx consumed, by asking Core —
/// for each input, does the wallet know the previous tx, and is the address
/// on that prevout ours? (`gettransaction` + `getaddressinfo`.) No local
/// UTXO mirror, no amount-based guessing.
fn process_candidate_spend(
    conn: &Connection,
    rpc: &RpcClient,
    w: &WalletRow,
    spending_txid: &str,
) -> Result<(), String> {
    // No "already evented?" shortcut here on purpose: one transaction can
    // consume outpoints from SEVERAL watched wallets (or several of one
    // wallet's, half-recorded before a crash), and a tx-level early exit
    // silently drops the rest. `record_spend` dedups per outpoint via
    // idx_event_dedup, which is the correct granularity; the cost is a few
    // redundant lookups until the scan floor advances past the tx.

    let d = rpc
        .get_transaction(&w.core_ref, spending_txid, true)
        .map_err(|e| e.to_string())?;
    let confirmations = d["confirmations"].as_i64().unwrap_or(0);
    let vins = d["decoded"]["vin"].as_array().cloned().unwrap_or_default();

    let mut spent: Vec<SpentOutpoint> = Vec::new();
    for vin in vins {
        let (ptxid, pvout) = match (vin["txid"].as_str(), vin["vout"].as_i64()) {
            (Some(t), Some(v)) => (t.to_string(), v),
            _ => continue, // coinbase or malformed
        };
        // Does this wallet know the prev tx at all? If not, the input can't
        // be ours (a wallet knows every tx that pays it).
        let prev = match rpc.get_transaction(&w.core_ref, &ptxid, true) {
            Ok(p) => p,
            Err(crate::rpc::RpcError::Node { code: -5, .. }) => continue,
            Err(e) => return Err(e.to_string()),
        };
        let vout_obj = &prev["decoded"]["vout"][pvout as usize];
        let addr = vout_obj["scriptPubKey"]["address"].as_str().unwrap_or("");
        if addr.is_empty() {
            continue;
        }
        let info = rpc
            .call(Some(&w.core_ref), "getaddressinfo", serde_json::json!([addr]))
            .map_err(|e| e.to_string())?;
        let is_mine = info["ismine"].as_bool().unwrap_or(false)
            || info["iswatchonly"].as_bool().unwrap_or(false);
        if is_mine {
            spent.push(SpentOutpoint {
                txid: ptxid,
                vout: pvout,
                amount_sat: btc_to_sat(vout_obj["value"].as_f64().unwrap_or(0.0)),
            });
        }
    }

    for op in spent {
        record_spend(conn, w.id, &w.label, &op, spending_txid, confirmations)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Record one detected spend of a watched outpoint. Pure DB — unit-testable
/// without a node. Single transaction (invariant 18): event insert, preauth
/// retirement, and the alarm enqueue commit or roll back together.
///
/// Returns Some(event_id) for a newly recorded spend, None if deduped.
pub fn record_spend(
    conn: &Connection,
    wallet_id: i64,
    wallet_label: &str,
    op: &SpentOutpoint,
    spending_txid: &str,
    confirmations: i64,
) -> rusqlite::Result<Option<i64>> {
    let now = timeutil::now();
    let conf_state = confirmation_state(confirmations, final_depth(conn));

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> rusqlite::Result<Option<i64>> {
        // dedup (idempotent across ZMQ/RPC double hits)
        let dup: Option<i64> = conn
            .query_row(
                "SELECT id FROM event WHERE spent_txid=?1 AND spent_vout=?2 AND spending_txid=?3",
                params![op.txid, op.vout, spending_txid],
                |r| r.get(0),
            )
            .optional()?;
        if dup.is_some() {
            return Ok(None);
        }

        // THE theft check: does this exact outpoint match a pending pre-auth?
        let preauth: Option<i64> = conn
            .query_row(
                "SELECT id FROM preauth WHERE txid=?1 AND vout=?2 AND state='pending'",
                params![op.txid, op.vout],
                |r| r.get(0),
            )
            .optional()?;

        let authorized = preauth.is_some();
        let auto_resolve = authorized && db::config_get_i64(conn, "authorized_auto_resolve") == 1;
        let resolution = if auto_resolve { "auto_resolved" } else { "firing" };

        conn.execute(
            "INSERT INTO event(wallet_id,spent_txid,spent_vout,amount_sat,spending_txid,
                               confirmation,confirmations,resolution,authorized,preauth_id,
                               detected_at,last_escalated_at,resolved_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11,?12)",
            params![
                wallet_id,
                op.txid,
                op.vout,
                op.amount_sat,
                spending_txid,
                conf_state,
                confirmations.max(0),
                resolution,
                authorized as i64,
                preauth,
                now,
                if auto_resolve { Some(now) } else { None },
            ],
        )?;
        let event_id = conn.last_insert_rowid();

        if let Some(pid) = preauth {
            // natural retirement: single-use, terminal (invariant 1 makes
            // reuse impossible; this just records it)
            conn.execute(
                "UPDATE preauth SET state='consumed', resolved_at=?1 WHERE id=?2",
                params![now, pid],
            )?;
        }

        if authorized {
            info!(
                "benign spend (pre-authorized): wallet '{wallet_label}' outpoint spent, {} BTC",
                format_btc(op.amount_sat)
            );
            // §8.3: a consumed pre-auth is a reconciliation checkpoint; the
            // worker runs it (needs RPC) right after this commits.
            db::meta_set(conn, &format!("checkpoint_due:{wallet_id}"), &now.to_string())?;
            if db::config_get_i64(conn, "log_authorized_spends") == 1 {
                let p = outbox::payload(
                    outbox::TIER_HEARTBEAT,
                    "authorized_spend",
                    &format!("Authorized spend from {wallet_label}"),
                    &format!(
                        "Pre-authorized UTXO ({} BTC) was spent as expected. \
                         Change re-enters the watched set automatically. \
                         A balance confirmation prompt will follow.",
                        format_btc(op.amount_sat)
                    ),
                    Some(wallet_label),
                    Some(event_id),
                    Some(op.amount_sat),
                    None,
                );
                outbox::enqueue(conn, outbox::TIER_HEARTBEAT, "authorized_spend",
                                Some(wallet_id), Some(event_id), &p)?;
            }
        } else {
            // ALARM. Mint the single-use capability token (sign on the way
            // out, §10.2); only its hash is stored. A mint failure (missing
            // token key) means the ack path is broken — that must be LOUD,
            // not a silent empty token, but it never blocks the alarm itself
            // (fail toward can't-dismiss, invariant 11).
            let token = match auth::mint_cap_token(conn, event_id) {
                Some(t) => t,
                None => {
                    log::error!(
                        "capability token mint FAILED for event {event_id} — \
                         acknowledge will not work until secrets are repaired \
                         (satlanced rotate-secrets)"
                    );
                    crate::degraded::raise(
                        conn,
                        "cap_token_key_missing",
                        "The capability-token key is missing from the DB; alarms fire but cannot \
                         be acknowledged. Run `satlanced rotate-secrets` (daemon stopped) to repair.",
                    );
                    String::new()
                }
            };
            conn.execute(
                "UPDATE event SET cap_token_hash=?1 WHERE id=?2",
                params![auth::sha256_hex(&token), event_id],
            )?;
            warn!(
                "ALARM: unauthorized spend detected — wallet '{wallet_label}', {} BTC, event {event_id}",
                format_btc(op.amount_sat)
            );
            let p = outbox::payload(
                outbox::TIER_ALARM,
                "spend_alarm",
                &format!("⚠ UNAUTHORIZED SPEND — {wallet_label}"),
                &format!(
                    "A watched UTXO worth {} BTC was spent with NO matching \
                     pre-authorization ({}). If this was not you, your keys may be \
                     compromised — act now. If it was you, acknowledge to stop escalation.",
                    format_btc(op.amount_sat),
                    conf_state_human(conf_state)
                ),
                Some(wallet_label),
                Some(event_id),
                Some(op.amount_sat),
                Some(&token),
            );
            outbox::enqueue(conn, outbox::TIER_ALARM, "spend_alarm",
                            Some(wallet_id), Some(event_id), &p)?;
        }
        Ok(Some(event_id))
    })();

    match &result {
        Ok(_) => conn.execute_batch("COMMIT")?,
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    result
}

fn final_depth(conn: &Connection) -> i64 {
    db::config_get_i64(conn, "final_depth").max(1)
}

pub fn confirmation_state(confirmations: i64, final_depth: i64) -> &'static str {
    if confirmations >= final_depth {
        "final"
    } else if confirmations >= 1 {
        "confirmed"
    } else {
        "seen"
    }
}

fn conf_state_human(s: &str) -> &'static str {
    match s {
        "seen" => "seen in mempool, unconfirmed",
        "confirmed" => "confirmed on-chain",
        "final" => "final",
        _ => "status unknown",
    }
}

/// What the node currently says about a spending tx, as plain data so the
/// transition decision below is a pure, unit-testable function.
pub struct TxView {
    /// The wallet still knows the tx (`gettransaction` succeeded).
    pub known: bool,
    /// Confirmations per the wallet; meaningless when `known` is false.
    pub confirmations: i64,
    /// Node-level mempool presence; None = probe unavailable/failed.
    pub in_mempool: Option<bool>,
    /// Seconds since the event was detected.
    pub age_secs: i64,
}

pub enum ConfAction {
    None,
    /// Tx is gone for real (conflicted, expunged, or aged out of the
    /// mempool): close the event (EDGE 6).
    Vanish,
    /// EDGE 5: confirmed then un-confirmed by a reorg.
    Reorged,
    /// Normal progress to N >= 1 confirmations.
    Advance(i64),
    /// A vanished spend came back (re-broadcast or finally mined). The
    /// alarm must re-open — a closed event must never mask a live theft.
    Resurrect(i64),
}

/// The confirmation-axis transition decision (§8.6), pure. Key property:
/// **age alone never vanishes a still-live tx.** A spend kept alive in the
/// mempool past the timeout (trivial for an attacker: re-broadcast) must
/// keep ringing, and one that vanished then reappears must ring again.
pub fn decide_confirmation(old_state: &str, tx: &TxView, vanished_after: i64) -> ConfAction {
    if !tx.known {
        // Wallet no longer knows it (dropped/abandoned). Give it the full
        // window before declaring it gone — transient wallet states happen.
        return if old_state != "vanished" && tx.age_secs > vanished_after {
            ConfAction::Vanish
        } else {
            ConfAction::None
        };
    }
    if tx.confirmations < 0 {
        // Conflicted: a different tx spent the inputs — this one can never
        // confirm. Definitive regardless of age.
        return if old_state == "vanished" { ConfAction::None } else { ConfAction::Vanish };
    }
    if tx.confirmations == 0 {
        return match old_state {
            "confirmed" => ConfAction::Reorged,
            // back in the mempool at 0-conf after we closed it: live again
            "vanished" if tx.in_mempool == Some(true) => ConfAction::Resurrect(0),
            "vanished" => ConfAction::None,
            // seen/reorged at 0-conf: vanish only when aged out AND the
            // node's mempool confirms absence (probe failure ⇒ keep waiting)
            _ if tx.age_secs > vanished_after && tx.in_mempool == Some(false) => {
                ConfAction::Vanish
            }
            _ => ConfAction::None,
        };
    }
    // confirmations >= 1
    if old_state == "vanished" {
        ConfAction::Resurrect(tx.confirmations)
    } else {
        ConfAction::Advance(tx.confirmations)
    }
}

/// Confirmation-axis tracking (§8.6): poll non-final events (including
/// `vanished` ones, so resurrections are caught) against Core and apply
/// `decide_confirmation`. Emits status updates only at configured
/// `notify_depths`.
pub fn update_confirmations(conn: &Connection, rpc: &RpcClient) {
    let depths: Vec<i64> = db::config_get(conn, "notify_depths")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| vec![0, 1, 6]);
    let final_d = final_depth(conn);
    let vanished_after = db::config_get_i64(conn, "vanished_timeout_secs");
    let now = timeutil::now();

    let rows: Vec<(i64, i64, String, String, i64, String, i64, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT e.id, e.wallet_id, w.label, w.core_wallet_ref, e.confirmations,
                    e.confirmation, e.detected_at, e.spending_txid
             FROM event e JOIN wallet w ON w.id=e.wallet_id
             WHERE e.confirmation != 'final'",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        match stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                    r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
                ))
            })
            .and_then(|m| m.collect())
        {
            Ok(v) => v,
            Err(_) => return,
        }
    };

    for (eid, wid, wlabel, wref, old_conf, old_state, detected_at, spending_txid) in rows {
        let (known, c) = match rpc.get_transaction(&wref, &spending_txid, false) {
            Ok(t) => (true, t["confirmations"].as_i64().unwrap_or(0)),
            Err(crate::rpc::RpcError::Node { code: -5, .. }) => (false, 0),
            Err(_) => continue, // node trouble; degraded handling covers it
        };
        let age = now - detected_at;
        // Mempool probe only where the decision needs it (0-conf cases).
        let in_mempool = if known && c == 0 {
            rpc.in_mempool(&spending_txid).ok()
        } else {
            None
        };
        let view = TxView { known, confirmations: c, in_mempool, age_secs: age };

        match decide_confirmation(&old_state, &view, vanished_after) {
            ConfAction::None => {}
            ConfAction::Vanish => transition_vanished(conn, eid, wid, &wlabel),
            ConfAction::Reorged => {
                let _ = conn.execute(
                    "UPDATE event SET confirmation='reorged', confirmations=0 WHERE id=?1",
                    [eid],
                );
                let p = outbox::payload(
                    outbox::TIER_DEGRADED, "event_reorged",
                    &format!("Reorg affected spend event #{eid}"),
                    &format!("A confirmed spend from {wlabel} was reorged out; re-evaluating. \
                              It may re-confirm or vanish."),
                    Some(&wlabel), Some(eid), None, None,
                );
                let _ = outbox::enqueue(conn, outbox::TIER_DEGRADED, "event_reorged",
                                        Some(wid), Some(eid), &p);
            }
            ConfAction::Resurrect(c) => transition_resurrected(conn, eid, wid, &wlabel, c, final_d),
            ConfAction::Advance(c) => {
                let new_state = confirmation_state(c, final_d);
                let _ = conn.execute(
                    "UPDATE event SET confirmation=?1, confirmations=?2 WHERE id=?3",
                    params![new_state, c, eid],
                );
                // notify on crossing configured depths (0 was emitted at creation)
                for d in depths.iter().filter(|d| **d > 0 && old_conf < **d && c >= **d) {
                    let kind = "confirmation_update";
                    let p = outbox::payload(
                        outbox::TIER_HEARTBEAT, kind,
                        &format!("Spend event #{eid}: {d} confirmation(s)"),
                        &format!(
                            "The spend from {wlabel} now has {c} confirmation(s){}.",
                            if new_state == "final" { " — FINAL" } else { "" }
                        ),
                        Some(&wlabel), Some(eid), None, None,
                    );
                    let _ = outbox::enqueue(conn, outbox::TIER_HEARTBEAT, kind, Some(wid), Some(eid), &p);
                }
            }
        }
    }
}

/// A vanished spend reappeared (re-broadcast / mined after we closed the
/// event). Re-open the alarm if the close was OUR doing (`auto_resolved`);
/// an event a human acknowledged stays acknowledged. Single transaction
/// (invariant 18).
fn transition_resurrected(
    conn: &Connection,
    event_id: i64,
    wallet_id: i64,
    wlabel: &str,
    confirmations: i64,
    final_d: i64,
) {
    let new_state = if confirmations >= 1 {
        confirmation_state(confirmations, final_d)
    } else {
        "seen"
    };
    let _ = conn.execute_batch("BEGIN IMMEDIATE");
    let _ = conn.execute(
        "UPDATE event SET confirmation=?1, confirmations=?2 WHERE id=?3",
        params![new_state, confirmations.max(0), event_id],
    );
    let reopened = conn
        .execute(
            "UPDATE event SET resolution='firing', resolved_at=NULL, last_escalated_at=?1
             WHERE id=?2 AND resolution='auto_resolved'",
            params![timeutil::now(), event_id],
        )
        .unwrap_or(0);
    if reopened > 0 {
        warn!("ALARM REOPENED: vanished spend on '{wlabel}' is back (event {event_id}, {confirmations} conf)");
        let token = auth::mint_cap_token(conn, event_id);
        let p = outbox::payload(
            outbox::TIER_ALARM,
            "spend_alarm_reopened",
            &format!("⚠ SPEND RE-APPEARED — {wlabel}"),
            &format!(
                "A spend that previously vanished from the mempool is back \
                 ({}). The alarm has re-opened and will escalate until \
                 acknowledged.",
                if confirmations >= 1 { "now confirmed on-chain" } else { "back in the mempool" }
            ),
            Some(wlabel),
            Some(event_id),
            None,
            token.as_deref(),
        );
        let _ = outbox::enqueue(conn, outbox::TIER_ALARM, "spend_alarm_reopened",
                                Some(wallet_id), Some(event_id), &p);
    }
    let _ = conn.execute_batch("COMMIT");
}

/// EDGE 6: a firing event whose confirmation axis hits `vanished` is
/// auto-resolved — nothing on-chain remains to alarm about.
fn transition_vanished(conn: &Connection, event_id: i64, wallet_id: i64, wlabel: &str) {
    let _ = conn.execute_batch("BEGIN IMMEDIATE");
    let _ = conn.execute(
        "UPDATE event SET confirmation='vanished' WHERE id=?1",
        [event_id],
    );
    let _ = conn.execute(
        "UPDATE event SET resolution='auto_resolved', resolved_at=?1
         WHERE id=?2 AND resolution='firing'",
        params![timeutil::now(), event_id],
    );
    let p = outbox::payload(
        outbox::TIER_HEARTBEAT, "event_vanished",
        &format!("Spend event #{event_id} vanished"),
        &format!("The mempool spend from {wlabel} was never confirmed and has been \
                  dropped. The event is closed (auto-resolved, noted as vanished)."),
        Some(wlabel), Some(event_id), None, None,
    );
    let _ = outbox::enqueue(conn, outbox::TIER_HEARTBEAT, "event_vanished",
                            Some(wallet_id), Some(event_id), &p);
    let _ = conn.execute_batch("COMMIT");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use crate::db;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        db::init_schema(&c).unwrap();
        db::config_seed_defaults(&c).unwrap();
        db::meta_set(&c, auth::META_TOKEN_KEY, "test-key").unwrap();
        c.execute(
            "INSERT INTO wallet(label,core_wallet_ref,arming_state,added_at)
             VALUES('cold','cold-ref','armed',0)",
            [],
        )
        .unwrap();
        outbox::register_notifier(&c, "ntfy", &["alarm".into(), "heartbeat".into()], Some(300))
            .unwrap();
        c
    }

    fn op(txid: &str, vout: i64, sat: i64) -> SpentOutpoint {
        SpentOutpoint { txid: txid.into(), vout, amount_sat: sat }
    }

    #[test]
    fn unauthorized_spend_alarms_with_capability_token() {
        let c = mem();
        let eid = record_spend(&c, 1, "cold", &op("aa", 0, 18_700_000), "bb", 0)
            .unwrap()
            .unwrap();
        let (res, auth_flag): (String, i64) = c
            .query_row("SELECT resolution, authorized FROM event WHERE id=?1", [eid], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(res, "firing");
        assert_eq!(auth_flag, 0);
        // alarm enqueued on alarm tier, payload carries token + ack path
        let (tier, payload): (String, String) = c
            .query_row("SELECT tier,payload FROM outbox", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(tier, "alarm");
        let p: Value = serde_json::from_str(&payload).unwrap();
        let token = p["capability_token"].as_str().unwrap();
        assert!(auth::verify_cap_token(&c, eid, token));
        // stored form is the hash, not the token
        let stored: String = c
            .query_row("SELECT cap_token_hash FROM event WHERE id=?1", [eid], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, auth::sha256_hex(token));
        assert_ne!(stored, token);
    }

    #[test]
    fn preauthorized_spend_is_benign_and_single_use() {
        let c = mem();
        c.execute(
            "INSERT INTO preauth(wallet_id,txid,vout,state,created_at,expires_at)
             VALUES(1,'aa',0,'pending',0,9999999999)",
            [],
        )
        .unwrap();
        let eid = record_spend(&c, 1, "cold", &op("aa", 0, 5000), "bb", 0)
            .unwrap()
            .unwrap();
        let (res, authorized, pid): (String, i64, Option<i64>) = c
            .query_row(
                "SELECT resolution,authorized,preauth_id FROM event WHERE id=?1",
                [eid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(res, "auto_resolved");
        assert_eq!(authorized, 1);
        assert_eq!(pid, Some(1));
        let pstate: String = c
            .query_row("SELECT state FROM preauth WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pstate, "consumed");
        // no alarm-tier message
        let alarms: i64 = c
            .query_row("SELECT COUNT(*) FROM outbox WHERE tier='alarm'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(alarms, 0);
        // checkpoint flagged for the worker
        assert!(db::meta_get(&c, "checkpoint_due:1").is_some());

        // a SECOND spend of a *different* outpoint still alarms: the consumed
        // preauth must not bless anything else (M5 done-when check)
        let eid2 = record_spend(&c, 1, "cold", &op("cc", 1, 7000), "dd", 0)
            .unwrap()
            .unwrap();
        let res2: String = c
            .query_row("SELECT resolution FROM event WHERE id=?1", [eid2], |r| r.get(0))
            .unwrap();
        assert_eq!(res2, "firing");
    }

    #[test]
    fn double_hit_is_one_event_not_two_panics() {
        let c = mem();
        let first = record_spend(&c, 1, "cold", &op("aa", 0, 1), "bb", 0).unwrap();
        let second = record_spend(&c, 1, "cold", &op("aa", 0, 1), "bb", 1).unwrap();
        assert!(first.is_some());
        assert!(second.is_none(), "ZMQ-then-RPC double hit must dedupe");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let alarms: i64 = c
            .query_row("SELECT COUNT(*) FROM outbox WHERE tier='alarm'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(alarms, 1);
    }

    #[test]
    fn confirmation_state_mapping() {
        assert_eq!(confirmation_state(0, 6), "seen");
        assert_eq!(confirmation_state(1, 6), "confirmed");
        assert_eq!(confirmation_state(5, 6), "confirmed");
        assert_eq!(confirmation_state(6, 6), "final");
        assert_eq!(confirmation_state(100, 6), "final");
    }

    #[test]
    fn detection_ignores_amounts_for_matching() {
        // same amount, different outpoint -> still an alarm (identity, not value)
        let c = mem();
        c.execute(
            "INSERT INTO preauth(wallet_id,txid,vout,state,created_at,expires_at)
             VALUES(1,'aa',0,'pending',0,9999999999)",
            [],
        )
        .unwrap();
        let eid = record_spend(&c, 1, "cold", &op("aa", 1, 5000), "bb", 0) // vout differs!
            .unwrap()
            .unwrap();
        let res: String = c
            .query_row("SELECT resolution FROM event WHERE id=?1", [eid], |r| r.get(0))
            .unwrap();
        assert_eq!(res, "firing", "outpoint (txid,vout) identity is the matcher");
    }

    #[test]
    fn multi_outpoint_spend_records_every_outpoint() {
        // One tx consuming several watched outpoints (cross-wallet or
        // intra-wallet) must produce one event PER outpoint — dedup is by
        // (spent_txid, spent_vout, spending_txid), never by tx alone.
        let c = mem();
        let e1 = record_spend(&c, 1, "cold", &op("aa", 0, 1000), "bb", 0).unwrap();
        let e2 = record_spend(&c, 1, "cold", &op("cc", 2, 2000), "bb", 0).unwrap();
        assert!(e1.is_some() && e2.is_some(), "second outpoint of the same tx must not be dropped");
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM event WHERE spending_txid='bb'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn vanish_decision_never_closes_a_live_tx() {
        let after = 100;
        let v = |old: &str, tx: &TxView| decide_confirmation(old, tx, after);
        let mk = |known, c, mem: Option<bool>, age| TxView {
            known, confirmations: c, in_mempool: mem, age_secs: age,
        };
        // aged but STILL IN MEMPOOL: must not vanish (attacker re-broadcasts)
        assert!(matches!(v("seen", &mk(true, 0, Some(true), 999)), ConfAction::None));
        // aged and mempool probe failed: don't guess, keep waiting
        assert!(matches!(v("seen", &mk(true, 0, None, 999)), ConfAction::None));
        // aged and confirmed-absent: vanish
        assert!(matches!(v("seen", &mk(true, 0, Some(false), 999)), ConfAction::Vanish));
        // conflicted: vanish immediately regardless of age
        assert!(matches!(v("seen", &mk(true, -1, None, 1)), ConfAction::Vanish));
        // wallet forgot it, aged: vanish; recent: wait
        assert!(matches!(v("seen", &mk(false, 0, None, 999)), ConfAction::Vanish));
        assert!(matches!(v("seen", &mk(false, 0, None, 1)), ConfAction::None));
        // confirmed -> 0-conf = reorg (EDGE 5)
        assert!(matches!(v("confirmed", &mk(true, 0, None, 1)), ConfAction::Reorged));
        // vanished then mined: resurrect; vanished back in mempool: resurrect
        assert!(matches!(v("vanished", &mk(true, 3, None, 999)), ConfAction::Resurrect(3)));
        assert!(matches!(v("vanished", &mk(true, 0, Some(true), 999)), ConfAction::Resurrect(0)));
        // vanished and still gone: stay closed
        assert!(matches!(v("vanished", &mk(true, 0, Some(false), 999)), ConfAction::None));
        assert!(matches!(v("vanished", &mk(false, 0, None, 999)), ConfAction::None));
        // normal progress
        assert!(matches!(v("seen", &mk(true, 2, None, 1)), ConfAction::Advance(2)));
    }

    #[test]
    fn resurrected_spend_reopens_auto_resolved_but_not_acknowledged() {
        let c = mem();
        // auto_resolved (vanish-closed) event comes back -> firing + alarm
        c.execute(
            "INSERT INTO event(wallet_id,spent_txid,spent_vout,amount_sat,spending_txid,
             confirmation,resolution,detected_at,resolved_at)
             VALUES(1,'aa',0,1000,'bb','vanished','auto_resolved',0,50)",
            [],
        )
        .unwrap();
        transition_resurrected(&c, 1, 1, "cold", 2, 6);
        let (conf, res): (String, String) = c
            .query_row("SELECT confirmation,resolution FROM event WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(conf, "confirmed");
        assert_eq!(res, "firing", "a closed event must never mask a live theft");
        let alarms: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE kind='spend_alarm_reopened'",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alarms, 1);

        // acknowledged event comes back -> confirmation updates, but the
        // human's acknowledgement stands (no re-fire)
        c.execute(
            "INSERT INTO event(wallet_id,spent_txid,spent_vout,amount_sat,spending_txid,
             confirmation,resolution,detected_at,resolved_at)
             VALUES(1,'cc',0,1000,'dd','vanished','acknowledged',0,50)",
            [],
        )
        .unwrap();
        transition_resurrected(&c, 2, 1, "cold", 1, 6);
        let res2: String = c
            .query_row("SELECT resolution FROM event WHERE id=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(res2, "acknowledged");
        let alarms2: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE kind='spend_alarm_reopened'",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alarms2, 1, "acknowledged events must not re-fire");
    }

    #[test]
    fn payload_never_contains_addresses_or_descriptors() {
        let c = mem();
        record_spend(&c, 1, "cold", &op("aa", 0, 1000), "bb", 0).unwrap();
        let payload: String = c.query_row("SELECT payload FROM outbox", [], |r| r.get(0)).unwrap();
        let p: Value = serde_json::from_str(&payload).unwrap();
        for k in p.as_object().unwrap().keys() {
            assert!(
                !["address", "descriptor", "xpub"].contains(&k.as_str()),
                "payload must structurally exclude sensitive material"
            );
        }
    }
}
