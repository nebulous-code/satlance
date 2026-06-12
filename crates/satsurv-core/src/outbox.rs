//! Outbox (design §9). The core decides *that* a message goes out and records
//! *whether* it did; never *how*. Channel-blind: the only things it knows
//! about a notifier are its label, tier subscription, and ack promise.
//!
//! Fan-out, not claim (invariant 15): enqueue eagerly materializes one
//! `outbox_delivery` row per notifier registered for the message's tier.
//! Zero rows = zero coverage (detectable); a stuck `pending` row = a
//! delinquent notifier (§9.3). No exclusive leases.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::timeutil;

pub const TIER_ALARM: &str = "alarm";
pub const TIER_DEGRADED: &str = "degraded";
pub const TIER_HEARTBEAT: &str = "heartbeat";

/// Enqueue one message and fan it out. Returns (outbox_id, delivery_rows).
/// Callers needing multi-row atomicity (invariant 18) run this inside their
/// own transaction; plain `Connection` works for standalone messages too.
pub fn enqueue(
    conn: &Connection,
    tier: &str,
    kind: &str,
    wallet_id: Option<i64>,
    event_id: Option<i64>,
    payload: &Value,
) -> rusqlite::Result<(i64, usize)> {
    let now = timeutil::now();
    conn.execute(
        "INSERT INTO outbox(tier,kind,wallet_id,event_id,payload,created_at)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![tier, kind, wallet_id, event_id, payload.to_string(), now],
    )?;
    let outbox_id = conn.last_insert_rowid();

    // Eager fan-out to every notifier subscribed to this tier.
    let mut stmt = conn.prepare(
        "SELECT n.id, n.ack_window_secs FROM notifier n
         JOIN notifier_tier t ON t.notifier_id = n.id
         WHERE t.tier = ?1",
    )?;
    let subs: Vec<(i64, Option<i64>)> = stmt
        .query_map([tier], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (nid, window) in &subs {
        // NULL ack_window = explicitly declared fire-and-forget (invariant 10):
        // expected_ack_by stays NULL, opting out of delinquency detection.
        let expected = window.map(|w| now + w);
        conn.execute(
            "INSERT INTO outbox_delivery(outbox_id,notifier_id,state,expected_ack_by)
             VALUES(?1,?2,'pending',?3)",
            params![outbox_id, nid, expected],
        )?;
    }
    Ok((outbox_id, subs.len()))
}

/// Channel-agnostic payload (§9.1 / §4): everything ANY channel needs to
/// render — title, body, tier, stable references, and the capability token
/// when the notification invites a verb. Amounts appear only as message
/// content (human-facing), never as detection inputs (invariant 2).
#[allow(clippy::too_many_arguments)]
pub fn payload(
    tier: &str,
    kind: &str,
    title: &str,
    body: &str,
    wallet_label: Option<&str>,
    event_id: Option<i64>,
    amount_sat: Option<i64>,
    capability_token: Option<&str>,
) -> Value {
    let mut p = json!({
        "tier": tier,
        "kind": kind,
        "title": title,
        "body": body,
        "created_at": timeutil::iso8601(timeutil::now()),
    });
    let o = p.as_object_mut().unwrap();
    if let Some(w) = wallet_label {
        o.insert("wallet_label".into(), json!(w));
    }
    if let Some(e) = event_id {
        o.insert("event_id".into(), json!(e));
        o.insert("ack_path".into(), json!(format!("/events/{e}/acknowledge")));
    }
    if let Some(a) = amount_sat {
        o.insert("amount_sat".into(), json!(a));
    }
    if let Some(t) = capability_token {
        o.insert("capability_token".into(), json!(t));
    }
    p
}

/// Pending delivery rows for one notifier (its long-poll view).
pub fn pending_for(conn: &Connection, notifier_id: i64) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT d.id, d.outbox_id, o.tier, o.kind, o.payload, o.created_at, d.state
         FROM outbox_delivery d JOIN outbox o ON o.id = d.outbox_id
         WHERE d.notifier_id = ?1 AND d.state != 'delivered'
         ORDER BY o.created_at ASC LIMIT 50",
    )?;
    let rows = stmt
        .query_map([notifier_id], |r| {
            let payload_str: String = r.get(4)?;
            Ok(json!({
                "delivery_id": r.get::<_, i64>(0)?,
                "outbox_id": r.get::<_, i64>(1)?,
                "tier": r.get::<_, String>(2)?,
                "kind": r.get::<_, String>(3)?,
                "payload": serde_json::from_str::<Value>(&payload_str)
                    .unwrap_or(Value::Null),
                "created_at": timeutil::iso8601(r.get::<_, i64>(5)?),
                "state": r.get::<_, String>(6)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Two-stage ack (§9.2): pending -> received -> delivered. Returns false if
/// the row isn't this notifier's or the transition is invalid.
pub fn ack(
    conn: &Connection,
    delivery_id: i64,
    notifier_id: i64,
    stage: &str, // "received" | "delivered"
) -> rusqlite::Result<bool> {
    let now = timeutil::now();
    let n = match stage {
        "received" => conn.execute(
            "UPDATE outbox_delivery SET state='received', received_at=?1
             WHERE id=?2 AND notifier_id=?3 AND state='pending'",
            params![now, delivery_id, notifier_id],
        )?,
        "delivered" => conn.execute(
            "UPDATE outbox_delivery SET state='delivered', delivered_at=?1
             WHERE id=?2 AND notifier_id=?3 AND state IN ('pending','received')",
            params![now, delivery_id, notifier_id],
        )?,
        _ => 0,
    };
    conn.execute(
        "UPDATE notifier SET last_seen_at=?1 WHERE id=?2",
        params![now, notifier_id],
    )?;
    Ok(n > 0)
}

/// Register (or re-register, idempotent by label) a notifier identity +
/// tier subscription + ack promise. The core learns identity, never nature
/// (invariant 13).
pub fn register_notifier(
    conn: &Connection,
    label: &str,
    tiers: &[String],
    ack_window_secs: Option<i64>,
) -> rusqlite::Result<i64> {
    let now = timeutil::now();
    conn.execute(
        "INSERT INTO notifier(label,ack_window_secs,registered_at,last_seen_at)
         VALUES(?1,?2,?3,?3)
         ON CONFLICT(label) DO UPDATE SET ack_window_secs=excluded.ack_window_secs,
                                          last_seen_at=excluded.last_seen_at",
        params![label, ack_window_secs, now],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM notifier WHERE label=?1",
        [label],
        |r| r.get(0),
    )?;
    conn.execute("DELETE FROM notifier_tier WHERE notifier_id=?1", [id])?;
    for t in tiers {
        conn.execute(
            "INSERT OR IGNORE INTO notifier_tier(notifier_id,tier) VALUES(?1,?2)",
            params![id, t],
        )?;
    }
    Ok(id)
}

/// Coverage check (§9.3 "no coverage"): tiers with zero registered notifiers.
pub fn uncovered_tiers(conn: &Connection) -> Vec<&'static str> {
    [TIER_ALARM, TIER_DEGRADED, TIER_HEARTBEAT]
        .into_iter()
        .filter(|t| {
            conn.query_row(
                "SELECT COUNT(*) FROM notifier_tier WHERE tier=?1",
                [*t],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n == 0)
            .unwrap_or(true)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        db::init_schema(&c).unwrap();
        c
    }

    #[test]
    fn fan_out_one_row_per_subscriber_none_blocks() {
        let c = mem();
        let a = register_notifier(&c, "ntfy", &["alarm".into(), "heartbeat".into()], Some(300)).unwrap();
        let b = register_notifier(&c, "email", &["alarm".into()], None).unwrap();
        let (_oid, n) = enqueue(&c, TIER_ALARM, "spend_alarm", None, None, &json!({})).unwrap();
        assert_eq!(n, 2);
        // each gets its own pending row
        assert_eq!(pending_for(&c, a).unwrap().len(), 1);
        assert_eq!(pending_for(&c, b).unwrap().len(), 1);
        // one acking doesn't touch the other (fan-out, not claim)
        let da = pending_for(&c, a).unwrap()[0]["delivery_id"].as_i64().unwrap();
        assert!(ack(&c, da, a, "received").unwrap());
        assert!(ack(&c, da, a, "delivered").unwrap());
        assert_eq!(pending_for(&c, a).unwrap().len(), 0);
        assert_eq!(pending_for(&c, b).unwrap().len(), 1);
        // a notifier can't ack another notifier's row
        let dbid = pending_for(&c, b).unwrap()[0]["delivery_id"].as_i64().unwrap();
        assert!(!ack(&c, dbid, a, "delivered").unwrap());
    }

    #[test]
    fn fire_and_forget_opts_out_of_ack_clock() {
        let c = mem();
        register_notifier(&c, "faf", &["heartbeat".into()], None).unwrap();
        enqueue(&c, TIER_HEARTBEAT, "heartbeat", None, None, &json!({})).unwrap();
        let exp: Option<i64> = c
            .query_row("SELECT expected_ack_by FROM outbox_delivery", [], |r| r.get(0))
            .unwrap();
        assert!(exp.is_none(), "explicit NULL window must mean no delinquency clock");
    }

    #[test]
    fn tier_routing_only_subscribers_get_rows() {
        let c = mem();
        register_notifier(&c, "hb-only", &["heartbeat".into()], Some(60)).unwrap();
        let (_o, n) = enqueue(&c, TIER_ALARM, "spend_alarm", None, None, &json!({})).unwrap();
        assert_eq!(n, 0, "no alarm subscribers -> zero rows = visible coverage gap");
        assert_eq!(uncovered_tiers(&c), vec![TIER_ALARM, TIER_DEGRADED]);
    }

    #[test]
    fn payload_carries_ack_affordance() {
        let p = payload(
            TIER_ALARM, "spend_alarm", "t", "b",
            Some("coldcard-main"), Some(7), Some(18_700_000), Some("tok"),
        );
        assert_eq!(p["ack_path"], "/events/7/acknowledge");
        assert_eq!(p["amount_sat"], 18_700_000);
        assert_eq!(p["capability_token"], "tok");
    }
}
