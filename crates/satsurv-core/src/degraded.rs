//! Degraded-state handling (design §7.1, §9.1).
//!
//! One row per active condition in the `degraded` table; conditions raise,
//! re-notify (~1h, consolidated presentation), recheck (~1min), and clear
//! with a one-shot recovery notice. The core reports honestly and never
//! papers over a coverage gap (invariant 16).
//!
//! Conditions used:
//!   node_unreachable          — RPC down past grace → blind_all_armed (inv. 7)
//!   node_ibd                  — node syncing; can't trust wallet queries
//!   zmq_silent                — trigger path quiet → RPC-poll "slow mode"
//!   notifier_delinquent:<lbl> — registered notifier stopped pulling (§9.3)
//!   tier_uncovered:<tier>     — no notifier registered for a tier

use log::{error, info, warn};
use rusqlite::{params, Connection};

use crate::rpc::RpcClient;
use crate::{arming, db, outbox, timeutil};

/// Raise a condition (idempotent). First raise enqueues a degraded notice.
pub fn raise(conn: &Connection, key: &str, detail: &str) {
    let now = timeutil::now();
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO degraded(key,detail,started_at,last_notified_at)
             VALUES(?1,?2,?3,?3)",
            params![key, detail, now],
        )
        .unwrap_or(0);
    if inserted == 0 {
        return; // already active — renotify sweep owns reminders
    }
    warn!("DEGRADED raised: {key} — {detail}");
    let p = outbox::payload(
        outbox::TIER_DEGRADED,
        "degraded_raised",
        &format!("DEGRADED: {key}"),
        detail,
        None,
        None,
        None,
        None,
    );
    let _ = outbox::enqueue(conn, outbox::TIER_DEGRADED, "degraded_raised", None, None, &p);
}

/// Clear a condition. One-shot recovery notice (config `recovery_notice`).
pub fn clear(conn: &Connection, key: &str, recovery_detail: &str) {
    let removed = conn
        .execute("DELETE FROM degraded WHERE key=?1", [key])
        .unwrap_or(0);
    if removed == 0 {
        return;
    }
    info!("DEGRADED cleared: {key}");
    if db::config_get_i64(conn, "recovery_notice") == 1 {
        let p = outbox::payload(
            outbox::TIER_DEGRADED,
            "degraded_cleared",
            &format!("Recovered: {key}"),
            recovery_detail,
            None,
            None,
            None,
            None,
        );
        let _ = outbox::enqueue(conn, outbox::TIER_DEGRADED, "degraded_cleared", None, None, &p);
    }
}

pub fn is_active(conn: &Connection, key: &str) -> bool {
    conn.query_row("SELECT 1 FROM degraded WHERE key=?1", [key], |_| Ok(()))
        .is_ok()
}

pub fn active_conditions(conn: &Connection) -> Vec<(String, String, i64)> {
    let mut stmt = match conn.prepare("SELECT key, detail, started_at FROM degraded ORDER BY started_at") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Consolidated re-notify sweep (~1h): conditions raise/clear independently,
/// but the *reminder* is one message listing everything still broken.
pub fn renotify_sweep(conn: &Connection) {
    let interval = db::config_get_i64(conn, "degraded_renotify_secs");
    let now = timeutil::now();
    let due: Vec<(String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT key, detail FROM degraded WHERE last_notified_at <= ?1",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([now - interval], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };
    if due.is_empty() {
        return;
    }
    // Reminder consolidates ALL active conditions, not just the due ones.
    let all = active_conditions(conn);
    let body = all
        .iter()
        .map(|(k, d, since)| format!("- {k} (since {}): {d}", timeutil::iso8601(*since)))
        .collect::<Vec<_>>()
        .join("\n");
    let p = outbox::payload(
        outbox::TIER_DEGRADED,
        "degraded_reminder",
        &format!("Still DEGRADED ({} condition(s))", all.len()),
        &body,
        None,
        None,
        None,
        None,
    );
    let _ = outbox::enqueue(conn, outbox::TIER_DEGRADED, "degraded_reminder", None, None, &p);
    let _ = conn.execute("UPDATE degraded SET last_notified_at=?1", [now]);
}

/// Node watchdog: probes RPC, manages node_unreachable/node_ibd, and keeps
/// `armed` honest (invariant 7) via blind/unblind. Grace window (§13
/// node_grace_secs) rides out a node restart before declaring blindness.
/// Returns true when the node is usable (reachable + past IBD).
pub fn node_watchdog(conn: &Connection, rpc: &RpcClient, network: &str) -> bool {
    let now = timeutil::now();
    match rpc.get_blockchain_info() {
        Ok(info) => {
            db::meta_set(conn, "node_last_ok", &now.to_string()).ok();
            if is_active(conn, "node_unreachable") {
                clear(conn, "node_unreachable", "Node reachable again. Re-arming watched wallets and running catch-up detection.");
                let n = arming::unblind_all(conn);
                if n > 0 {
                    info!("unblinded {n} wallet(s) after node recovery");
                }
            }
            // IBD check — regtest is exempt (always "synced" semantically).
            let ibd = network != "regtest" && info["initialblockdownload"].as_bool().unwrap_or(false);
            if ibd {
                raise(conn, "node_ibd", "Node is in initial block download; wallet state is not trustworthy until sync completes.");
                false
            } else {
                if is_active(conn, "node_ibd") {
                    clear(conn, "node_ibd", "Node sync complete.");
                }
                true
            }
        }
        Err(e) => {
            let last_ok: i64 = db::meta_get(conn, "node_last_ok")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let grace = db::config_get_i64(conn, "node_grace_secs");
            if now - last_ok > grace && !is_active(conn, "node_unreachable") {
                error!("node unreachable past grace window: {e}");
                let n = arming::blind_all_armed(conn);
                raise(
                    conn,
                    "node_unreachable",
                    &format!(
                        "Cannot reach the Bitcoin node ({n} wallet(s) flipped to BLINDED — NOT PROTECTED). \
                         This is a visibility loss, not a detected theft."
                    ),
                );
            }
            false
        }
    }
}

/// §9.3 notifier delinquency: a notifier that promised an ack window but has
/// a pending/received delivery past `expected_ack_by` is flagged BY NAME on
/// the degraded tier. Fire-and-forget notifiers (ack_window NULL) are exempt
/// by declaration (invariant 10).
pub fn delinquency_sweep(conn: &Connection) {
    let now = timeutil::now();
    let delinquent: Vec<String> = {
        let mut stmt = match conn.prepare(
            "SELECT DISTINCT n.label FROM outbox_delivery d
             JOIN notifier n ON n.id = d.notifier_id
             WHERE d.state IN ('pending','received')
               AND d.expected_ack_by IS NOT NULL
               AND d.expected_ack_by < ?1",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([now], |r| r.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };
    let delinquent_set: std::collections::HashSet<&str> =
        delinquent.iter().map(|s| s.as_str()).collect();

    for label in &delinquent {
        raise(
            conn,
            &format!("notifier_delinquent:{label}"),
            &format!("Notifier '{label}' has overdue deliveries (promised ack window missed). Its channel may be down."),
        );
    }
    // Clear recovered notifiers.
    for (key, _, _) in active_conditions(conn) {
        if let Some(label) = key.strip_prefix("notifier_delinquent:") {
            if !delinquent_set.contains(label) {
                clear(conn, &key, &format!("Notifier '{label}' is delivering again."));
            }
        }
    }
}

/// Coverage honesty: tiers with zero registered notifiers are surfaced as
/// degraded (the core reports the gap; it never reroutes around it — inv. 16).
pub fn coverage_sweep(conn: &Connection) {
    let uncovered = outbox::uncovered_tiers(conn);
    let uncovered_set: std::collections::HashSet<&str> = uncovered.iter().copied().collect();
    for tier in &uncovered {
        raise(
            conn,
            &format!("tier_uncovered:{tier}"),
            &format!("No notifier is registered for the '{tier}' tier. Messages on this tier queue but reach no one."),
        );
    }
    for (key, _, _) in active_conditions(conn) {
        if let Some(tier) = key.strip_prefix("tier_uncovered:") {
            if !uncovered_set.contains(tier) {
                clear(conn, &key, &format!("Tier '{tier}' is covered again."));
            }
        }
    }
}

/// Disk/SQLite write failure → ALARM tier immediately (§7.1): a daemon that
/// can't persist can't be trusted to remember an alarm. Best-effort enqueue
/// (the write may itself fail — the /health endpoint is the true backstop).
pub fn disk_write_alarm(conn: &Connection, detail: &str) {
    error!("DISK/DB WRITE FAILURE: {detail}");
    let p = outbox::payload(
        outbox::TIER_ALARM,
        "disk_write_failure",
        "DAEMON CANNOT PERSIST STATE",
        &format!("A database write failed: {detail}. The daemon cannot be trusted to remember alarms. Investigate the host immediately."),
        None,
        None,
        None,
        None,
    );
    let _ = outbox::enqueue(conn, outbox::TIER_ALARM, "disk_write_failure", None, None, &p);
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
    fn raise_is_idempotent_and_clear_emits_recovery() {
        let c = mem();
        raise(&c, "zmq_silent", "no messages");
        raise(&c, "zmq_silent", "no messages"); // second raise: no new notice
        let notices: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE kind='degraded_raised'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(notices, 1);
        assert!(is_active(&c, "zmq_silent"));

        clear(&c, "zmq_silent", "back");
        assert!(!is_active(&c, "zmq_silent"));
        let rec: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE kind='degraded_cleared'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rec, 1);
        // clearing a non-active condition is a no-op (no second recovery)
        clear(&c, "zmq_silent", "back");
        let rec2: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE kind='degraded_cleared'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rec2, 1);
    }

    #[test]
    fn renotify_consolidates_all_active() {
        let c = mem();
        raise(&c, "a", "first");
        raise(&c, "b", "second");
        // force both due
        c.execute("UPDATE degraded SET last_notified_at=0", []).unwrap();
        renotify_sweep(&c);
        let body: String = c
            .query_row(
                "SELECT payload FROM outbox WHERE kind='degraded_reminder'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(body.contains("a") && body.contains("b"));
        // and not due again immediately
        renotify_sweep(&c);
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE kind='degraded_reminder'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn delinquency_flags_by_name_and_recovers() {
        let c = mem();
        let nid = outbox::register_notifier(&c, "ntfy", &["alarm".into()], Some(300)).unwrap();
        c.execute(
            "INSERT INTO outbox(tier,kind,payload,created_at) VALUES('alarm','x','{}',0)",
            [],
        )
        .unwrap();
        let oid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO outbox_delivery(outbox_id,notifier_id,state,expected_ack_by)
             VALUES(?1,?2,'pending',1)",
            params![oid, nid],
        )
        .unwrap();
        delinquency_sweep(&c);
        assert!(is_active(&c, "notifier_delinquent:ntfy"));
        // notifier catches up
        c.execute("UPDATE outbox_delivery SET state='delivered'", []).unwrap();
        delinquency_sweep(&c);
        assert!(!is_active(&c, "notifier_delinquent:ntfy"));
    }
}
