//! The worker: single owner of all node-facing and time-driven work.
//!
//! One thread, one loop. It receives triggers over an mpsc channel (ZMQ poke,
//! gap, API nudge) and otherwise wakes on a timeout tick. Every wake runs the
//! same set of duties — trigger or tick only changes *latency*, never *what
//! gets done* (ZMQ is a trigger, not truth; the RPC pass is authoritative and
//! self-healing, invariant 4).
//!
//! Duties per wake (gated by per-duty cadence):
//!   - node watchdog (blinded honesty), detection pass, confirmation updates
//!   - preauth expiry sweep, pending checkpoints, onboarding poll
//!   - escalation (repeat unacked alarms), delinquency, coverage, renotify
//!   - backstop reconciliation, heartbeat digest, ZMQ-silence watch

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{info, warn};
use rusqlite::{params, Connection};

use crate::rpc::RpcClient;
use crate::sats::format_btc;
use crate::{arming, db, degraded, detect, outbox, reconcile, timeutil};

#[derive(Debug)]
pub enum Trigger {
    /// ZMQ message arrived: something happened on-chain — go look.
    Zmq,
    /// ZMQ sequence gap detected: same response, logged separately.
    ZmqGap,
    /// ZMQ recv timeout: quiet period (liveness accounting only).
    ZmqIdle,
    /// A control verb changed state (e.g. dispute); re-run duties promptly.
    Nudge,
}

const TICK_SECS: u64 = 30;

/// What a tip change says about ZMQ health, if anything.
///
/// `None` means "no conclusion": either no block arrived, or this is the
/// first tip we have recorded and we cannot know whether ZMQ announced it.
/// `Some(true)` means a block arrived that ZMQ never announced.
///
/// Deliberately says nothing about elapsed time. Silence on a quiet chain is
/// correct behaviour, not a fault (invariant 2: ZMQ triggers, RPC decides —
/// a missed message may delay, never corrupt).
pub(crate) fn zmq_observation(
    last_seen_tip: &str,
    tip: &str,
    zmq_since_tip_change: bool,
) -> Option<bool> {
    if tip.is_empty() || tip == last_seen_tip || last_seen_tip.is_empty() {
        return None;
    }
    Some(!zmq_since_tip_change)
}

pub fn run(conn: Arc<Mutex<Connection>>, rpc: RpcClient, network: String, rx: Receiver<Trigger>) {
    info!("worker: started (tick {}s)", TICK_SECS);
    // ZMQ health is tracked per BLOCK, not per second (see the check below).
    let mut last_seen_tip = String::new();
    let mut zmq_since_tip_change = false;
    let mut last_duties: i64 = 0;
    let mut last_backstop: i64 = 0;
    let mut last_degraded_recheck: i64 = 0;
    // Heartbeat schedule survives restarts (meta), so a crash-looping daemon
    // can't spam digests — and a MISSED heartbeat stays a meaningful signal.
    let mut last_heartbeat: i64 = {
        let c = conn.lock().expect("db mutex");
        db::meta_get(&c, "last_heartbeat_at")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };

    // One echo pass shortly after a ZMQ-triggered cycle: zmqpubrawtx can
    // publish before the node's wallets finish indexing the tx, so the
    // triggered pass may query a hair too early. The echo closes that race
    // without reverting to a constant poll storm.
    let mut followup_at: Option<i64> = None;

    loop {
        // Wait for a trigger or the tick (shortened when an echo is due).
        // Drain any pile-up so a burst of ZMQ messages coalesces into one
        // pass instead of N redundant ones.
        let wait_secs = match followup_at {
            Some(t) => (t - timeutil::now()).clamp(1, TICK_SECS as i64) as u64,
            None => TICK_SECS,
        };
        let trigger = rx.recv_timeout(Duration::from_secs(wait_secs));
        let mut real_work = false; // something happened on-chain / via API
        match &trigger {
            Ok(Trigger::Zmq) | Ok(Trigger::ZmqGap) | Ok(Trigger::Nudge) => real_work = true,
            Ok(Trigger::ZmqIdle) => {}
            Err(RecvTimeoutError::Timeout) => real_work = true,
            Err(RecvTimeoutError::Disconnected) => {
                info!("worker: trigger channel closed — exiting");
                return;
            }
        }
        let mut zmq_activity = matches!(&trigger, Ok(Trigger::Zmq) | Ok(Trigger::ZmqGap));
        while let Ok(t) = rx.try_recv() {
            if matches!(t, Trigger::Zmq | Trigger::ZmqGap) {
                zmq_activity = true;
                real_work = true;
            }
            if matches!(t, Trigger::Nudge) {
                real_work = true;
            }
        }
        let now = timeutil::now();
        if zmq_activity {
            zmq_since_tip_change = true;
        }
        let followup_due = followup_at.map_or(false, |t| now >= t);
        // The ZMQ thread's 5s recv timeout sends ZmqIdle just to keep this
        // loop's liveness accounting current. Idle pokes alone must not run
        // the duty cycle, or the "30s tick floor" becomes a 5s RPC-poll storm.
        if !real_work && !followup_due && now - last_duties < TICK_SECS as i64 {
            continue;
        }
        if followup_due {
            followup_at = None;
        }
        if zmq_activity {
            followup_at = Some(now + 2);
        }
        last_duties = now;

        let conn = match conn.lock() {
            Ok(c) => c,
            Err(_) => {
                warn!("worker: db mutex poisoned — exiting");
                return;
            }
        };

        // --- node honesty first: nothing else is trustworthy if this fails.
        let node_ok = degraded::node_watchdog(&conn, &rpc, &network);
        if node_ok {
            // --- the detector (invariants 2,3): authoritative RPC pass.
            if let Err(e) = detect::detection_pass(&conn, &rpc) {
                warn!("detection pass error: {e}");
            }
            detect::update_confirmations(&conn, &rpc);
            arming::poll_onboarding(&conn, &rpc);
            reconcile::finish_pending_rescans(&conn, &rpc);
            process_due_checkpoints(&conn, &rpc);

            // --- backstop reconciliation, cheap cadence (~5min).
            if now - last_backstop >= 300 {
                reconcile::backstop_check(&conn, &rpc);
                last_backstop = now;
            }
        }

        // --- preauth expiry (works without the node; pure clock).
        preauth_expiry_sweep(&conn);

        // --- escalation: repeat-until-acked alarms (invariant 11 asymmetry —
        //     ringing is relentless; only an authenticated ack stops it).
        escalation_sweep(&conn);

        // --- degraded machinery on its recheck cadence (~1min default).
        let recheck = db::config_get_i64(&conn, "degraded_recheck_secs");
        if now - last_degraded_recheck >= recheck {
            degraded::delinquency_sweep(&conn);
            degraded::coverage_sweep(&conn);
            degraded::renotify_sweep(&conn);
            // ZMQ health, judged per block rather than per second.
            //
            // The old check raised after 3 quiet ticks (90s). But ZMQ message
            // arrival is driven by chain and mempool activity, which varies by
            // orders of magnitude between networks: on mainnet 90s of silence
            // means something broke, while on signet the mean block gap is
            // ~10min and quiet is the normal state. A signet soak produced 38
            // raise/clear pairs in two weeks, none of them real.
            //
            // The useful question is not "has ZMQ been quiet" but "did ZMQ
            // tell us about the blocks RPC can see". A block arriving without
            // a matching trigger is a genuine trigger-path failure on any
            // network; no block arriving is not evidence of anything. This
            // also detects a dead ZMQ faster than a timeout would, since it
            // fires on the first block rather than after a fixed wait.
            if node_ok && network != "regtest" {
                let tip = db::meta_get(&conn, "last_block_hash").unwrap_or_default();
                if let Some(missed) = zmq_observation(&last_seen_tip, &tip, zmq_since_tip_change) {
                    let need = db::config_get_i64(&conn, "zmq_missed_blocks");
                    degraded::observe(
                        &conn,
                        "zmq_silent",
                        missed,
                        "Blocks are arriving without ZMQ announcing them. Falling back to RPC polling (higher latency; detection still covered by the periodic pass).",
                        "ZMQ trigger path is live again.",
                        need,
                    );
                }
                if !tip.is_empty() && tip != last_seen_tip {
                    last_seen_tip = tip;
                    zmq_since_tip_change = false;
                }
            }
            last_degraded_recheck = now;
        }

        // --- heartbeat ("silence is itself an alarm", §9).
        let hb_interval = db::config_get_i64(&conn, "heartbeat_secs");
        if now - last_heartbeat >= hb_interval {
            heartbeat(&conn);
            last_heartbeat = now;
            let _ = db::meta_set(&conn, "last_heartbeat_at", &now.to_string());
        }
    }
}

/// Pre-auth expiry (§8.4 sweep): pending past TTL → expired, re-arm notice,
/// reconciliation checkpoint flagged ("balance unchanged" still worth a
/// confirm — both endings checkpoint, §8.3).
fn preauth_expiry_sweep(conn: &Connection) {
    let now = timeutil::now();
    let due: Vec<(i64, i64, String, i64, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT p.id, p.wallet_id, p.txid, p.vout, w.label
             FROM preauth p JOIN wallet w ON w.id = p.wallet_id
             WHERE p.state='pending' AND p.expires_at < ?1",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([now], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };
    for (pid, wid, txid, vout, label) in due {
        let n = conn
            .execute(
                "UPDATE preauth SET state='expired', resolved_at=?1 WHERE id=?2 AND state='pending'",
                params![now, pid],
            )
            .unwrap_or(0);
        if n == 0 {
            continue;
        }
        info!("preauth {pid} on '{label}' expired unspent — UTXO re-armed");
        let p = outbox::payload(
            outbox::TIER_DEGRADED,
            "preauth_lapsed",
            &format!("Pre-authorization lapsed: '{label}'"),
            &format!(
                "Pre-auth for outpoint {}…:{vout} expired unspent. The UTXO is re-armed; \
                 spending it now WILL alarm. Balance unchanged.",
                &txid[..txid.len().min(12)]
            ),
            Some(&label),
            None,
            None,
            None,
        );
        let _ = outbox::enqueue(conn, outbox::TIER_DEGRADED, "preauth_lapsed", Some(wid), None, &p);
        // Both endings checkpoint (§8.3).
        let _ = db::meta_set(conn, &format!("checkpoint_due:{wid}"), &now.to_string());
    }
}

/// Run reconciliation checkpoints flagged by detect.rs (consumed preauth) or
/// the expiry sweep, after their transactions committed.
fn process_due_checkpoints(conn: &Connection, rpc: &RpcClient) {
    let due: Vec<i64> = {
        let mut stmt = match conn.prepare(
            "SELECT key FROM meta WHERE key LIKE 'checkpoint_due:%'",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([], |r| r.get::<_, String>(0))
            .map(|rows| {
                rows.filter_map(|r| r.ok())
                    .filter_map(|k| k.strip_prefix("checkpoint_due:").and_then(|s| s.parse().ok()))
                    .collect()
            })
            .unwrap_or_default()
    };
    for wid in due {
        reconcile::checkpoint(
            conn,
            rpc,
            wid,
            "A pre-authorization resolved — confirming keeps the baseline honest.",
        );
        let _ = conn.execute(
            "DELETE FROM meta WHERE key=?1",
            [format!("checkpoint_due:{wid}")],
        );
    }
}

/// Repeat-until-acked: any event still `firing` whose last escalation is
/// older than alarm_repeat_secs gets re-enqueued to the alarm tier (eager
/// fan-out to ALL alarm notifiers; the §13 staged-delay ladder is simplified
/// to immediate fan-out + repetition — documented in STATUS.md).
fn escalation_sweep(conn: &Connection) {
    let now = timeutil::now();
    let repeat = db::config_get_i64(conn, "alarm_repeat_secs");
    let due: Vec<(i64, i64, String, i64, String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT e.id, e.wallet_id, w.label, e.amount_sat, e.spent_txid, e.confirmation
             FROM event e JOIN wallet w ON w.id = e.wallet_id
             WHERE e.resolution='firing'
               AND (e.last_escalated_at IS NULL OR e.last_escalated_at <= ?1)",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([now - repeat], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };
    for (eid, wid, label, sat, spent_txid, conf) in due {
        let token = crate::auth::mint_cap_token(conn, eid);
        if token.is_none() {
            // Repeats keep ringing, but the ack path is broken — say so.
            degraded::raise(
                conn,
                "cap_token_key_missing",
                "The capability-token key is missing from the DB; alarms fire but cannot \
                 be acknowledged. Run `satlanced rotate-secrets` (daemon stopped) to repair.",
            );
        }
        let p = outbox::payload(
            outbox::TIER_ALARM,
            "spend_alarm_repeat",
            &format!("STILL FIRING: unauthorized spend on '{label}'"),
            &format!(
                "Unacknowledged alarm: watched UTXO {}… ({} BTC) was spent ({conf}). \
                 This alert repeats until acknowledged.",
                &spent_txid[..spent_txid.len().min(12)],
                format_btc(sat)
            ),
            Some(&label),
            Some(eid),
            Some(sat),
            token.as_deref(),
        );
        if outbox::enqueue(conn, outbox::TIER_ALARM, "spend_alarm_repeat", Some(wid), Some(eid), &p).is_ok() {
            let _ = conn.execute(
                "UPDATE event SET last_escalated_at=?1 WHERE id=?2",
                params![now, eid],
            );
        }
    }
}

/// The heartbeat digest (§9): balance notation per wallet (§8.3), loud
/// NOT PROTECTED section (invariant 9), DEGRADED flag when any condition is
/// active (never an all-clear that lies).
fn heartbeat(conn: &Connection) {
    let (armed, not_protected) = arming::status_lines(conn);
    let conditions = degraded::active_conditions(conn);
    let degraded_active = !conditions.is_empty();

    let mut body = String::new();
    if degraded_active {
        body.push_str("STATUS: DEGRADED\n");
        for (k, d, _) in &conditions {
            body.push_str(&format!("  ! {k}: {d}\n"));
        }
        body.push('\n');
    } else {
        body.push_str("STATUS: OK\n\n");
    }
    if armed.is_empty() && not_protected.is_empty() {
        body.push_str("No wallets registered.\n");
    }
    for line in &armed {
        body.push_str(line);
        body.push('\n');
    }
    for line in &not_protected {
        body.push_str(line);
        body.push('\n');
    }
    let firing: i64 = conn
        .query_row("SELECT COUNT(*) FROM event WHERE resolution='firing'", [], |r| r.get(0))
        .unwrap_or(0);
    if firing > 0 {
        body.push_str(&format!("\n{firing} alarm(s) STILL FIRING — acknowledge required.\n"));
    }

    let title = if degraded_active {
        "Sat Lance heartbeat — DEGRADED"
    } else {
        "Sat Lance heartbeat"
    };
    let p = outbox::payload(
        outbox::TIER_HEARTBEAT,
        "heartbeat",
        title,
        &body,
        None,
        None,
        None,
        None,
    );
    if let Err(e) = outbox::enqueue(conn, outbox::TIER_HEARTBEAT, "heartbeat", None, None, &p) {
        // Can't even write the heartbeat → the disk-failure alarm path (§7.1).
        degraded::disk_write_alarm(conn, &e.to_string());
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

    #[test]
    fn expired_preauth_rearms_and_checkpoints() {
        let c = mem();
        c.execute(
            "INSERT INTO wallet(label,core_wallet_ref,arming_state,added_at) VALUES('w','r','armed',0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO preauth(wallet_id,txid,vout,state,created_at,expires_at)
             VALUES(1,'aaaa',0,'pending',0,1)",
            [],
        )
        .unwrap();
        preauth_expiry_sweep(&c);
        let state: String = c
            .query_row("SELECT state FROM preauth WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "expired");
        // lapsed notice + checkpoint flag
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM outbox WHERE kind='preauth_lapsed'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(db::meta_get(&c, "checkpoint_due:1").is_some());
    }

    #[test]
    fn firing_event_repeats_until_acked() {
        let c = mem();
        c.execute(
            "INSERT INTO wallet(label,core_wallet_ref,arming_state,added_at) VALUES('w','r','armed',0)",
            [],
        )
        .unwrap();
        db::meta_set(&c, "cap_token_key", "deadbeef").unwrap();
        c.execute(
            "INSERT INTO event(wallet_id,spent_txid,spent_vout,amount_sat,spending_txid,
             resolution,detected_at,last_escalated_at) VALUES(1,'t',0,100,'s','firing',0,0)",
            [],
        )
        .unwrap();
        escalation_sweep(&c);
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM outbox WHERE kind='spend_alarm_repeat'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        // last_escalated_at bumped → not due again immediately
        escalation_sweep(&c);
        let n2: i64 = c
            .query_row("SELECT COUNT(*) FROM outbox WHERE kind='spend_alarm_repeat'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n2, 1);
        // acknowledged → never repeats again
        c.execute("UPDATE event SET resolution='acknowledged', last_escalated_at=0", []).unwrap();
        escalation_sweep(&c);
        let n3: i64 = c
            .query_row("SELECT COUNT(*) FROM outbox WHERE kind='spend_alarm_repeat'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n3, 1);
    }

    #[test]
    fn heartbeat_flips_to_degraded() {
        let c = mem();
        heartbeat(&c);
        degraded::raise(&c, "node_unreachable", "down");
        heartbeat(&c);
        let payloads: Vec<String> = {
            let mut stmt = c
                .prepare("SELECT payload FROM outbox WHERE kind='heartbeat' ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert!(payloads[0].contains("STATUS: OK"));
        assert!(payloads[1].contains("DEGRADED"));
    }

    #[test]
    fn zmq_health_is_judged_per_block_not_per_second() {
        // A quiet chain says nothing either way. This is the whole bug: the
        // old check raised after 90s of silence, and signet's mean block gap
        // is ~10min, so it fired constantly while nothing was wrong.
        assert_eq!(zmq_observation("aa", "aa", false), None, "no new block = no conclusion");
        assert_eq!(zmq_observation("", "aa", false), None, "first tip seen = no conclusion");
        assert_eq!(zmq_observation("aa", "", false), None, "no tip = no conclusion");

        // A block that arrived without a trigger is a real failure, on any
        // network, and is caught on the first block rather than after a wait.
        assert_eq!(zmq_observation("aa", "bb", false), Some(true));
        // A block the trigger path announced is health.
        assert_eq!(zmq_observation("aa", "bb", true), Some(false));
    }

    #[test]
    fn degraded_observe_debounces_both_directions() {
        let c = mem();
        let need = 2;

        // One observation is not enough to page anyone.
        degraded::observe(&c, "zmq_silent", true, "d", "r", need);
        assert!(!degraded::is_active(&c, "zmq_silent"), "single blip must not raise");

        // Two in a row is a real condition.
        degraded::observe(&c, "zmq_silent", true, "d", "r", need);
        assert!(degraded::is_active(&c, "zmq_silent"));

        // One good observation must not clear it either.
        degraded::observe(&c, "zmq_silent", false, "d", "r", need);
        assert!(degraded::is_active(&c, "zmq_silent"), "single recovery must not clear");

        degraded::observe(&c, "zmq_silent", false, "d", "r", need);
        assert!(!degraded::is_active(&c, "zmq_silent"));
    }

    #[test]
    fn flapping_observations_never_raise() {
        // The signet soak produced 38 raise/clear pairs, none real. Alternating
        // observations must produce exactly zero notifications.
        let c = mem();
        for i in 0..20 {
            degraded::observe(&c, "zmq_silent", i % 2 == 0, "d", "r", 3);
            assert!(!degraded::is_active(&c, "zmq_silent"), "flapping raised at i={i}");
        }
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "flapping must not enqueue any notification");
    }
}
