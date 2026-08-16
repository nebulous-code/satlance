//! State store (design §8). Single SQLite DB, WAL mode, single-writer daemon.
//!
//! The schema below is §8.2 verbatim, plus three documented extensions the
//! design implies but does not spell out as DDL:
//!   * `config`   — tunables (§13) live in SQLite, not a file (§4, §11).
//!   * `degraded` — "each [degraded] condition is its own tracked row with its
//!     own recheck and its own clear" (§9.1).
//!   * `event.cap_token_hash` / `cap_token_used` — per-notification capability
//!     token, "sign on the way out, verify on the way back" (§10.2). Only the
//!     HMAC's hash is stored; the token itself rides the outbox payload.
//!
//! Invariant 6: nothing in this schema can hold an xpub/descriptor/address.

use rusqlite::Connection;
use std::path::Path;

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // §10: tight file permissions — the cap-token key lives in `meta`.
    // Best-effort (the -wal/-shm sidecars inherit the dir; secrets files are
    // already 0600 from auth::init_secrets).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}

pub const SCHEMA: &str = r#"
-- The watched cold wallets. Parent entity; everything else references it.
CREATE TABLE IF NOT EXISTS wallet (
    id              INTEGER PRIMARY KEY,
    label           TEXT    NOT NULL UNIQUE,
    core_wallet_ref TEXT    NOT NULL,
    arming_state    TEXT    NOT NULL DEFAULT 'registered',
    reconciled_sat  INTEGER NOT NULL DEFAULT 0,
    reconciled_at   INTEGER,
    observed_sat    INTEGER,
    observed_at     INTEGER,
    observed_scans  INTEGER NOT NULL DEFAULT 0,
    observed_status TEXT    NOT NULL DEFAULT 'idle',
    added_at        INTEGER NOT NULL
);

-- Pending and historical pre-authorizations.
CREATE TABLE IF NOT EXISTS preauth (
    id            INTEGER PRIMARY KEY,
    wallet_id     INTEGER NOT NULL,
    txid          TEXT    NOT NULL,
    vout          INTEGER NOT NULL,
    state         TEXT    NOT NULL DEFAULT 'pending',
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    resolved_at   INTEGER,
    note          TEXT,
    FOREIGN KEY(wallet_id) REFERENCES wallet(id),
    UNIQUE(txid, vout, state)
);
CREATE INDEX IF NOT EXISTS idx_preauth_pending  ON preauth(state, expires_at);
CREATE INDEX IF NOT EXISTS idx_preauth_outpoint ON preauth(txid, vout);

-- Detected spends of watched UTXOs (the alarm records).
CREATE TABLE IF NOT EXISTS event (
    id                INTEGER PRIMARY KEY,
    wallet_id         INTEGER NOT NULL,
    spent_txid        TEXT    NOT NULL,
    spent_vout        INTEGER NOT NULL,
    amount_sat        INTEGER NOT NULL,
    spending_txid     TEXT    NOT NULL,
    confirmation      TEXT    NOT NULL DEFAULT 'seen',
    confirmations     INTEGER NOT NULL DEFAULT 0,
    resolution        TEXT    NOT NULL DEFAULT 'firing',
    authorized        INTEGER NOT NULL DEFAULT 0,
    preauth_id        INTEGER,
    detected_at       INTEGER NOT NULL,
    last_escalated_at INTEGER,
    resolved_at       INTEGER,
    -- §10.2 capability token (extension; see module doc):
    cap_token_hash    TEXT,
    cap_token_used    INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(wallet_id)  REFERENCES wallet(id),
    FOREIGN KEY(preauth_id) REFERENCES preauth(id)
);
CREATE INDEX IF NOT EXISTS idx_event_open     ON event(resolution, last_escalated_at);
CREATE INDEX IF NOT EXISTS idx_event_spending ON event(spending_txid);
CREATE UNIQUE INDEX IF NOT EXISTS idx_event_dedup ON event(spent_txid, spent_vout, spending_txid);

-- Singleton daemon position / misc state.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Tunables (§13). Config lives in SQLite, not a separate file.
CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Active degraded conditions (§9.1): one row per condition, independent
-- recheck/clear; only the re-notify *presentation* consolidates.
CREATE TABLE IF NOT EXISTS degraded (
    key              TEXT PRIMARY KEY,   -- e.g. 'node_unreachable', 'zmq_silent', 'notifier_delinquent:ntfy'
    detail           TEXT    NOT NULL,
    started_at       INTEGER NOT NULL,
    last_notified_at INTEGER
);

-- Registered notifiers (egress clients). Identity + tiers + ack promise.
CREATE TABLE IF NOT EXISTS notifier (
    id              INTEGER PRIMARY KEY,
    label           TEXT    NOT NULL UNIQUE,
    ack_window_secs INTEGER,
    registered_at   INTEGER NOT NULL,
    last_seen_at    INTEGER
);

CREATE TABLE IF NOT EXISTS notifier_tier (
    notifier_id     INTEGER NOT NULL,
    tier            TEXT    NOT NULL,
    PRIMARY KEY (notifier_id, tier),
    FOREIGN KEY(notifier_id) REFERENCES notifier(id)
);

-- The notification MESSAGE itself (one row), tier-stamped.
CREATE TABLE IF NOT EXISTS outbox (
    id            INTEGER PRIMARY KEY,
    tier          TEXT    NOT NULL,
    kind          TEXT    NOT NULL,
    wallet_id     INTEGER,
    event_id      INTEGER,
    payload       TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    FOREIGN KEY(wallet_id) REFERENCES wallet(id),
    FOREIGN KEY(event_id)  REFERENCES event(id)
);
CREATE INDEX IF NOT EXISTS idx_outbox_tier ON outbox(tier, created_at);

-- Per-(message x notifier) delivery tracking. FAN-OUT, not a claim.
CREATE TABLE IF NOT EXISTS outbox_delivery (
    id              INTEGER PRIMARY KEY,
    outbox_id       INTEGER NOT NULL,
    notifier_id     INTEGER NOT NULL,
    state           TEXT    NOT NULL DEFAULT 'pending',
    expected_ack_by INTEGER,
    received_at     INTEGER,
    delivered_at    INTEGER,
    FOREIGN KEY(outbox_id)   REFERENCES outbox(id),
    FOREIGN KEY(notifier_id) REFERENCES notifier(id),
    UNIQUE(outbox_id, notifier_id)
);
CREATE INDEX IF NOT EXISTS idx_delivery_open ON outbox_delivery(state, expected_ack_by);
"#;

// ---------------------------------------------------------------- meta

pub fn meta_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
        .ok()
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- config (§13 defaults)

pub const CONFIG_DEFAULTS: &[(&str, &str)] = &[
    ("heartbeat_secs", "21600"),          // 6h digest cadence
    ("preauth_ttl_secs", "86400"),        // 24h abandoned-intent expiry
    ("alarm_repeat_secs", "900"),         // repeat unacked alarm every 15min
    ("degraded_renotify_secs", "3600"),   // ~1h reminder while broken
    ("degraded_recheck_secs", "60"),      // ~1min condition retest
    ("recovery_notice", "1"),             // one-shot "protection restored"
    ("notify_depths", "[0,1,6]"),         // confirmation depths that emit updates
    ("final_depth", "6"),
    ("gap_limit_start", "1000"),
    ("gap_limit_cap", "10000"),
    ("vanished_timeout_secs", "1209600"), // 14d, matches Core mempool expiry
    ("notifier_default_ack_window_secs", "300"),
    ("outbox_longpoll_secs", "25"),
    ("node_grace_secs", "30"),            // ride out a node restart before 'blinded'
    // §14 minor decision, default chosen: benign authorized spends ARE logged
    // as events (auto_resolved) — audit trail over minimalism, consistent with
    // §13 "keep resolved events indefinitely (audit trail)".
    ("log_authorized_spends", "1"),
    // §8.4 config choice: authorized spends auto-close (no confirm nag).
    ("authorized_auto_resolve", "1"),
];

pub fn config_seed_defaults(conn: &Connection) -> rusqlite::Result<()> {
    for (k, v) in CONFIG_DEFAULTS {
        conn.execute(
            "INSERT OR IGNORE INTO config(key,value) VALUES(?1,?2)",
            [k, v],
        )?;
    }
    Ok(())
}

pub fn config_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM config WHERE key=?1", [key], |r| r.get(0))
        .ok()
}

pub fn config_get_i64(conn: &Connection, key: &str) -> i64 {
    config_get(conn, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            CONFIG_DEFAULTS
                .iter()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        config_seed_defaults(&c).unwrap();
        c
    }

    #[test]
    fn schema_creates_and_reopens_idempotently() {
        let c = mem();
        init_schema(&c).unwrap(); // idempotent
        assert_eq!(config_get_i64(&c, "heartbeat_secs"), 21600);
    }

    #[test]
    fn preauth_unique_pending_per_outpoint() {
        let c = mem();
        c.execute(
            "INSERT INTO wallet(label,core_wallet_ref,added_at) VALUES('w','w',0)",
            [],
        )
        .unwrap();
        let ins = "INSERT INTO preauth(wallet_id,txid,vout,state,created_at,expires_at)
                   VALUES(1,'aa',0,?1,0,100)";
        c.execute(ins, ["pending"]).unwrap();
        // second pending on same outpoint must be rejected
        assert!(c.execute(ins, ["pending"]).is_err());
        // but a historical consumed row + new pending coexist
        c.execute("UPDATE preauth SET state='consumed' WHERE id=1", [])
            .unwrap();
        c.execute(ins, ["pending"]).unwrap();
    }

    #[test]
    fn event_dedup_index_makes_double_hit_idempotent() {
        let c = mem();
        c.execute(
            "INSERT INTO wallet(label,core_wallet_ref,added_at) VALUES('w','w',0)",
            [],
        )
        .unwrap();
        let ins = "INSERT OR IGNORE INTO event
                   (wallet_id,spent_txid,spent_vout,amount_sat,spending_txid,detected_at)
                   VALUES(1,'aa',0,1000,'bb',0)";
        c.execute(ins, []).unwrap();
        c.execute(ins, []).unwrap(); // ZMQ-then-RPC double hit
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn meta_roundtrip() {
        let c = mem();
        meta_set(&c, "last_block_hash", "deadbeef").unwrap();
        assert_eq!(meta_get(&c, "last_block_hash").unwrap(), "deadbeef");
        meta_set(&c, "last_block_hash", "cafef00d").unwrap();
        assert_eq!(meta_get(&c, "last_block_hash").unwrap(), "cafef00d");
    }
}
