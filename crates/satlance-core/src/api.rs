//! The HTTP/JSON API (design §11.1, §11.2) — the single published surface
//! both contracts ride on. tiny_http, thread-per-request, synchronous; the
//! shared SQLite connection is single-writer behind a mutex.
//!
//! Auth (§10.2, invariants 11/12): role-scoped Bearer secrets enforced at the
//! credential. A notifier-scope secret on a verb/query endpoint → 403 — a
//! notifier is PHYSICALLY incapable of issuing a verb. The acknowledge verb
//! additionally requires the per-event single-use capability token (silencing
//! is hard; ringing is easy). /health is unauthenticated by design (the
//! all-channels-down backstop watcher must not need a secret).
//!
//! Payload conventions (§11.2): snake_case, integer sats, ISO-8601 on the
//! wire, `{"error":{"code","message"}}` with stable codes, 202 for async
//! lifecycle actions. Wallet payloads structurally exclude descriptors —
//! the struct has no such field, so it cannot leak.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::auth::Role;
use crate::rpc::RpcClient;
use crate::worker::Trigger;
use crate::{arming, auth, db, degraded, outbox, reconcile, timeutil};

pub struct Ctx {
    pub conn: Arc<Mutex<Connection>>,
    pub rpc: RpcClient,
    pub nudge: Sender<Trigger>,
    pub version: &'static str,
}

pub fn serve(bind: &str, ctx: Arc<Ctx>) -> std::io::Result<()> {
    let server = Server::http(bind).map_err(|e| std::io::Error::new(std::io::ErrorKind::AddrInUse, e.to_string()))?;
    info!("api: listening on http://{bind} (localhost-bound; use `tailscale serve` for remote access)");
    let server = Arc::new(server);
    // Small fixed pool: requests are short except the long-poll, which
    // sleeps; a handful of threads covers one CLI + a few notifiers.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let server = Arc::clone(&server);
        let ctx = Arc::clone(&ctx);
        handles.push(std::thread::spawn(move || loop {
            match server.recv() {
                Ok(req) => handle(req, &ctx),
                Err(e) => {
                    warn!("api: recv error: {e}");
                    return;
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

// ---------------------------------------------------------------- plumbing

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let data = body.to_string().into_bytes();
    Response::from_data(data)
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

fn err(status: u16, code: &str, message: &str) -> (u16, Value) {
    (status, json!({"error": {"code": code, "message": message}}))
}

fn bearer(req: &Request) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer ").map(str::to_string))
}

fn read_body(req: &mut Request) -> Value {
    let mut s = String::new();
    let _ = req.as_reader().take(1 << 20).read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

/// Route table entry: which role may pass. Notifier endpoints also admit
/// control scope for display reads where the spec allows it.
#[derive(PartialEq)]
enum Scope {
    Open,         // /health
    Control,      // verbs + queries
    Notifier,     // register / pull / ack (control may also read pending? no — writes stay notifier)
}

fn handle(mut req: Request, ctx: &Ctx) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path: Vec<String> = url
        .split('?')
        .next()
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let parts: Vec<&str> = path.iter().map(|s| s.as_str()).collect();

    // --- scope decision per route group (§11.1 tags).
    let scope = match parts.as_slice() {
        ["health"] => Scope::Open,
        ["notifiers", "register"] | ["outbox", ..] => Scope::Notifier,
        _ => Scope::Control,
    };

    // --- authenticate (before touching the body).
    let role = {
        let secret = bearer(&req);
        match (&scope, secret) {
            (Scope::Open, _) => None,
            (_, None) => {
                let (s, b) = err(401, "unauthenticated", "Authorization: Bearer <secret> required");
                let _ = req.respond(json_response(s, &b));
                return;
            }
            (_, Some(sec)) => {
                let conn = ctx.conn.lock().unwrap();
                match auth::role_for_secret(&conn, &sec) {
                    Some(r) => Some(r),
                    None => {
                        let (s, b) = err(401, "unauthenticated", "unrecognized secret");
                        drop(conn);
                        let _ = req.respond(json_response(s, &b));
                        return;
                    }
                }
            }
        }
    };
    // Role-scope enforcement (invariant 12): wrong scope → 403, full stop.
    let scope_ok = match (&scope, &role) {
        (Scope::Open, _) => true,
        (Scope::Control, Some(Role::Control)) => true,
        (Scope::Notifier, Some(Role::Notifier)) => true,
        // control-scope may READ the outbox display (§11.1) — exactly
        // GET /outbox, not /outbox/pending (that's the notifier long-poll,
        // and it side-effects last_seen_at):
        (Scope::Notifier, Some(Role::Control)) => {
            method == Method::Get && parts == ["outbox"]
        }
        _ => false,
    };
    if !scope_ok {
        let (s, b) = err(403, "wrong_scope", "this credential's role cannot access this endpoint");
        let _ = req.respond(json_response(s, &b));
        return;
    }

    let body = if method == Method::Post || method == Method::Delete {
        read_body(&mut req)
    } else {
        Value::Null
    };

    let (status, resp) = route(&method, &parts, &url, &body, ctx);
    let _ = req.respond(json_response(status, &resp));
}

fn route(method: &Method, parts: &[&str], url: &str, body: &Value, ctx: &Ctx) -> (u16, Value) {
    use Method::*;
    match (method, parts) {
        (Get, ["health"]) => (200, json!({"ok": true})),
        (Get, ["status"]) => get_status(ctx),

        (Post, ["wallets"]) => post_wallet(ctx, body),
        (Get, ["wallets"]) => list_wallets(ctx),
        (Get, ["wallets", id]) => get_wallet(ctx, id),
        (Delete, ["wallets", id]) => delete_wallet(ctx, id),
        (Post, ["wallets", id, "import"]) => wallet_import(ctx, id, body),
        (Post, ["wallets", id, "arm"]) => wallet_arm(ctx, id),
        (Post, ["wallets", id, "disarm"]) => wallet_disarm(ctx, id),
        (Post, ["wallets", id, "rescan"]) => wallet_rescan(ctx, id),
        (Post, ["wallets", id, "balance", "confirm"]) => balance_confirm(ctx, id, body),
        (Post, ["wallets", id, "balance", "dispute"]) => wallet_rescan(ctx, id), // same machinery (§11.1)

        (Post, ["preauths"]) => post_preauth(ctx, body),
        (Get, ["preauths"]) => list_preauths(ctx),
        (Delete, ["preauths", id]) => cancel_preauth(ctx, id),

        (Get, ["events"]) => list_events(ctx, url),
        (Get, ["events", id]) => get_event(ctx, id),
        (Post, ["events", id, "acknowledge"]) => acknowledge(ctx, id, body),

        (Post, ["notifiers", "register"]) => notifier_register(ctx, body),
        (Get, ["outbox"]) => outbox_list(ctx, url),
        (Get, ["outbox", "pending"]) => outbox_pending(ctx, url),
        (Post, ["outbox", id, "received"]) => outbox_ack(ctx, id, body, "received"),
        (Post, ["outbox", id, "delivered"]) => outbox_ack(ctx, id, body, "delivered"),

        _ => err(404, "not_found", "no such endpoint"),
    }
}

fn parse_id(s: &str) -> Result<i64, (u16, Value)> {
    s.parse().map_err(|_| err(400, "bad_id", "id must be an integer"))
}

// ---------------------------------------------------------------- status

fn get_status(ctx: &Ctx) -> (u16, Value) {
    let conn = ctx.conn.lock().unwrap();
    let node = match ctx.rpc.get_blockchain_info() {
        Ok(i) => json!({
            "reachable": true,
            "chain": i["chain"],
            "blocks": i["blocks"],
            "initial_block_download": i["initialblockdownload"],
        }),
        Err(e) => json!({"reachable": false, "error": e.to_string()}),
    };
    let conditions: Vec<Value> = degraded::active_conditions(&conn)
        .into_iter()
        .map(|(k, d, since)| json!({"key": k, "detail": d, "since": timeutil::iso8601(since)}))
        .collect();
    let (armed, not_protected) = arming::status_lines(&conn);
    let firing: i64 = conn
        .query_row("SELECT COUNT(*) FROM event WHERE resolution='firing'", [], |r| r.get(0))
        .unwrap_or(0);
    (
        200,
        json!({
            "version": ctx.version,
            "node": node,
            "degraded": conditions,
            "armed": armed,
            "not_protected": not_protected,
            "firing_events": firing,
            "uncovered_tiers": outbox::uncovered_tiers(&conn),
        }),
    )
}

// ---------------------------------------------------------------- wallets

/// The wallet payload. Defined once; structurally excludes descriptor/xpub —
/// there is no field for them, so no code path can echo them (§11.2).
fn wallet_json(conn: &Connection, id: i64) -> Option<Value> {
    conn.query_row(
        "SELECT id,label,core_wallet_ref,arming_state,reconciled_sat,reconciled_at,
                observed_sat,observed_at,observed_scans,observed_status,added_at
         FROM wallet WHERE id=?1",
        [id],
        |r| {
            let rec_at: Option<i64> = r.get(5)?;
            let obs_at: Option<i64> = r.get(7)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "label": r.get::<_, String>(1)?,
                "core_wallet_ref": r.get::<_, String>(2)?,
                "arming_state": r.get::<_, String>(3)?,
                "reconciled_sat": r.get::<_, i64>(4)?,
                "reconciled_at": rec_at.map(timeutil::iso8601),
                "observed_sat": r.get::<_, Option<i64>>(6)?,
                "observed_at": obs_at.map(timeutil::iso8601),
                "observed_at_unix": obs_at,
                "observed_scans": r.get::<_, i64>(8)?,
                "observed_status": r.get::<_, String>(9)?,
                "added_at": timeutil::iso8601(r.get::<_, i64>(10)?),
            }))
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Pull descriptors out of a request body. Accepts `"descriptor": "wpkh(...)"`
/// or `"descriptors": ["...", "..."]`. The strings live only on this call
/// stack — forwarded to Core, never persisted, never echoed (invariants 5, 6).
fn descriptors_from_body(body: &Value) -> Vec<String> {
    if let Some(d) = body["descriptor"].as_str() {
        return vec![d.to_string()];
    }
    if let Some(arr) = body["descriptors"].as_array() {
        return arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    }
    Vec::new()
}

fn post_wallet(ctx: &Ctx, body: &Value) -> (u16, Value) {
    let label = match body["label"].as_str() {
        Some(l) if !l.is_empty() => l,
        _ => return err(400, "missing_label", "label is required"),
    };
    let core_ref = body["core_wallet_ref"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("satlance-{label}"));
    let conn = ctx.conn.lock().unwrap();
    let id = match arming::register(&conn, label, &core_ref) {
        Ok(id) => id,
        Err(_) => return err(409, "label_exists", "a wallet with that label already exists"),
    };
    // Automated-import path: descriptor(s) forwarded to Core, never persisted,
    // never echoed (§11.1). Goes out of scope at the end of this function.
    let descs = descriptors_from_body(body);
    if !descs.is_empty() {
        if let Err(e) = arming::begin_import(&conn, &ctx.rpc, id, &descs) {
            return err(502, "import_failed", &e);
        }
    }
    let _ = ctx.nudge.send(Trigger::Nudge);
    match wallet_json(&conn, id) {
        Some(w) => (202, w),
        None => err(500, "db_error", "wallet vanished after insert"),
    }
}

fn list_wallets(ctx: &Ctx) -> (u16, Value) {
    let conn = ctx.conn.lock().unwrap();
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM wallet ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    let list: Vec<Value> = ids.iter().filter_map(|&id| wallet_json(&conn, id)).collect();
    (200, json!(list))
}

fn get_wallet(ctx: &Ctx, id: &str) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let conn = ctx.conn.lock().unwrap();
    match wallet_json(&conn, id) {
        Some(w) => (200, w),
        None => err(404, "wallet_not_found", "no such wallet"),
    }
}

/// EDGE 4 policy (user decision, 2026-06-12): a wallet with history cannot
/// be deleted — events/pre-auths are the audit trail and the §13 retention
/// default keeps them indefinitely. Disarm instead. Only a wallet that never
/// produced a row can be removed cleanly.
fn delete_wallet(ctx: &Ctx, id: &str) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let conn = ctx.conn.lock().unwrap();
    if wallet_has_history(&conn, id) {
        return err(
            409,
            "wallet_has_history",
            "this wallet has events/pre-auths/notifications — the audit trail is kept; disarm the wallet instead of deleting it",
        );
    }
    let n = conn.execute("DELETE FROM wallet WHERE id=?1", [id]).unwrap_or(0);
    if n == 0 {
        return err(404, "wallet_not_found", "no such wallet");
    }
    (200, json!({"deleted": id}))
}

/// Any child rows referencing this wallet? (The FK constraints would block
/// the DELETE anyway; this exists to answer 409 honestly instead of a
/// misleading failure.)
fn wallet_has_history(conn: &Connection, wallet_id: i64) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM event   WHERE wallet_id=?1)
             OR EXISTS(SELECT 1 FROM preauth WHERE wallet_id=?1)
             OR EXISTS(SELECT 1 FROM outbox  WHERE wallet_id=?1)",
        [wallet_id],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n != 0)
    .unwrap_or(true) // on DB error, refuse rather than half-delete
}

fn wallet_import(ctx: &Ctx, id: &str, body: &Value) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let descs = descriptors_from_body(body);
    if descs.is_empty() {
        return err(400, "missing_descriptor", "descriptor is required for automated import");
    }
    let conn = ctx.conn.lock().unwrap();
    if let Err(e) = arming::begin_import(&conn, &ctx.rpc, id, &descs) {
        return err(502, "import_failed", &e);
    }
    let _ = ctx.nudge.send(Trigger::Nudge);
    match wallet_json(&conn, id) {
        Some(w) => (202, w),
        None => err(404, "wallet_not_found", "no such wallet"),
    }
}

fn wallet_arm(ctx: &Ctx, id: &str) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let conn = ctx.conn.lock().unwrap();
    match arming::arm(&conn, &ctx.rpc, id) {
        Ok(_) => match wallet_json(&conn, id) {
            Some(w) => (202, w),
            None => err(404, "wallet_not_found", "no such wallet"),
        },
        Err(code) => err(409, &code, "cannot arm from the current state"),
    }
}

fn wallet_disarm(ctx: &Ctx, id: &str) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let conn = ctx.conn.lock().unwrap();
    match arming::disarm(&conn, id) {
        Ok(()) => match wallet_json(&conn, id) {
            Some(w) => (200, w),
            None => err(404, "wallet_not_found", "no such wallet"),
        },
        Err(code) => {
            let status = if code == "wallet_not_found" { 404 } else { 409 };
            err(status, code, "cannot disarm from the current state")
        }
    }
}

/// Rescan / dispute: "no, keep scanning" — gap-limit bump + rescan + recompute
/// (§8.3). Synchronous at regtest scale; 202 because mainnet rescans aren't.
fn wallet_rescan(ctx: &Ctx, id: &str) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let conn = ctx.conn.lock().unwrap();
    match reconcile::dispute(&conn, &ctx.rpc, id) {
        Ok(()) => {
            let _ = ctx.nudge.send(Trigger::Nudge);
            match wallet_json(&conn, id) {
                Some(w) => (202, w),
                None => err(404, "wallet_not_found", "no such wallet"),
            }
        }
        Err(e) => {
            if e == "wallet_not_found" {
                err(404, "wallet_not_found", "no such wallet")
            } else {
                err(502, "rescan_failed", &e)
            }
        }
    }
}

fn balance_confirm(ctx: &Ctx, id: &str, body: &Value) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    // §11.1: confirm carries the observed_at it saw (stale-guard). Accept
    // ISO ("observed_at") or unix ("observed_at_unix").
    let at = body["observed_at_unix"].as_i64().or_else(|| {
        body["observed_at"].as_str().and_then(timeutil::parse_iso8601)
    });
    let at = match at {
        Some(a) => a,
        None => return err(400, "missing_observed_at", "observed_at (ISO-8601) or observed_at_unix is required"),
    };
    let conn = ctx.conn.lock().unwrap();
    match reconcile::confirm(&conn, id, at) {
        Ok(_) => match wallet_json(&conn, id) {
            Some(w) => (200, w),
            None => err(404, "wallet_not_found", "no such wallet"),
        },
        Err(code) => {
            let status = match code {
                "wallet_not_found" => 404,
                "stale_observation" | "nothing_to_confirm" => 409,
                _ => 500,
            };
            err(status, code, "balance confirmation rejected")
        }
    }
}

// ---------------------------------------------------------------- preauths

fn post_preauth(ctx: &Ctx, body: &Value) -> (u16, Value) {
    let wallet_id = match body["wallet_id"].as_i64() {
        Some(w) => w,
        None => return err(400, "missing_wallet_id", "wallet_id is required"),
    };
    let txid = match body["txid"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return err(400, "missing_txid", "txid is required"),
    };
    let vout = match body["vout"].as_i64() {
        Some(v) if v >= 0 => v,
        _ => return err(400, "missing_vout", "vout is required"),
    };
    let note = body["note"].as_str();
    let conn = ctx.conn.lock().unwrap();
    // CONTRACTS.md says `ttl_secs` (matching ack_window_secs); `ttl` is kept
    // as an alias for older clients.
    let ttl = body["ttl_secs"]
        .as_i64()
        .or_else(|| body["ttl"].as_i64())
        .unwrap_or_else(|| db::config_get_i64(&conn, "preauth_ttl_secs"));
    let wallet_exists: bool = conn
        .query_row("SELECT 1 FROM wallet WHERE id=?1", [wallet_id], |_| Ok(()))
        .is_ok();
    if !wallet_exists {
        return err(404, "wallet_not_found", "no such wallet");
    }
    let now = timeutil::now();
    let res = conn.execute(
        "INSERT INTO preauth(wallet_id,txid,vout,state,created_at,expires_at,note)
         VALUES(?1,?2,?3,'pending',?4,?5,?6)",
        params![wallet_id, txid, vout, now, now + ttl, note],
    );
    match res {
        Ok(_) => {
            let pid = conn.last_insert_rowid();
            (201, preauth_json(&conn, pid).unwrap_or(json!({"id": pid})))
        }
        // UNIQUE(txid,vout,state) — at most one pending per outpoint (§8.5).
        Err(_) => err(409, "preauth_pending_exists", "a pending pre-auth already exists for that outpoint"),
    }
}

fn preauth_json(conn: &Connection, id: i64) -> Option<Value> {
    conn.query_row(
        "SELECT id,wallet_id,txid,vout,state,created_at,expires_at,resolved_at,note
         FROM preauth WHERE id=?1",
        [id],
        |r| {
            let resolved: Option<i64> = r.get(7)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "wallet_id": r.get::<_, i64>(1)?,
                "txid": r.get::<_, String>(2)?,
                "vout": r.get::<_, i64>(3)?,
                "state": r.get::<_, String>(4)?,
                "created_at": timeutil::iso8601(r.get::<_, i64>(5)?),
                "expires_at": timeutil::iso8601(r.get::<_, i64>(6)?),
                "resolved_at": resolved.map(timeutil::iso8601),
                "note": r.get::<_, Option<String>>(8)?,
            }))
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn list_preauths(ctx: &Ctx) -> (u16, Value) {
    let conn = ctx.conn.lock().unwrap();
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM preauth ORDER BY id DESC").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().filter_map(|r| r.ok()).collect()
    };
    (200, json!(ids.iter().filter_map(|&i| preauth_json(&conn, i)).collect::<Vec<_>>()))
}

fn cancel_preauth(ctx: &Ctx, id: &str) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let conn = ctx.conn.lock().unwrap();
    let now = timeutil::now();
    let n = conn
        .execute(
            "UPDATE preauth SET state='cancelled', resolved_at=?1 WHERE id=?2 AND state='pending'",
            params![now, id],
        )
        .unwrap_or(0);
    if n == 0 {
        return err(409, "preauth_not_pending", "no pending pre-auth with that id");
    }
    (200, preauth_json(&conn, id).unwrap_or(json!({"id": id, "state": "cancelled"})))
}

// ---------------------------------------------------------------- events

fn event_json(conn: &Connection, id: i64) -> Option<Value> {
    conn.query_row(
        "SELECT id,wallet_id,spent_txid,spent_vout,amount_sat,spending_txid,confirmation,
                confirmations,resolution,authorized,preauth_id,detected_at,resolved_at
         FROM event WHERE id=?1",
        [id],
        |r| {
            let resolved: Option<i64> = r.get(12)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "wallet_id": r.get::<_, i64>(1)?,
                "spent_txid": r.get::<_, String>(2)?,
                "spent_vout": r.get::<_, i64>(3)?,
                "amount_sat": r.get::<_, i64>(4)?,
                "spending_txid": r.get::<_, String>(5)?,
                "confirmation": r.get::<_, String>(6)?,
                "confirmations": r.get::<_, i64>(7)?,
                "resolution": r.get::<_, String>(8)?,
                "authorized": r.get::<_, i64>(9)? == 1,
                "preauth_id": r.get::<_, Option<i64>>(10)?,
                "detected_at": timeutil::iso8601(r.get::<_, i64>(11)?),
                "resolved_at": resolved.map(timeutil::iso8601),
            }))
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn list_events(ctx: &Ctx, url: &str) -> (u16, Value) {
    // filters: ?resolution=firing&confirmation=confirmed
    let mut resolution = None;
    let mut confirmation = None;
    if let Some(q) = url.split('?').nth(1) {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "resolution" => resolution = Some(v.to_string()),
                    "confirmation" => confirmation = Some(v.to_string()),
                    _ => {}
                }
            }
        }
    }
    let conn = ctx.conn.lock().unwrap();
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT id FROM event
                 WHERE (?1 IS NULL OR resolution=?1)
                   AND (?2 IS NULL OR confirmation=?2)
                 ORDER BY id DESC",
            )
            .unwrap();
        stmt.query_map(params![resolution, confirmation], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    (200, json!(ids.iter().filter_map(|&i| event_json(&conn, i)).collect::<Vec<_>>()))
}

fn get_event(ctx: &Ctx, id: &str) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let conn = ctx.conn.lock().unwrap();
    match event_json(&conn, id) {
        Some(e) => (200, e),
        None => err(404, "event_not_found", "no such event"),
    }
}

/// THE silencing verb (invariant 11): control scope AND the per-event,
/// single-use capability token. Without the token that rode out with the
/// alert, an authenticated control credential still cannot silence remotely-
/// presented alarms by id-guessing.
fn acknowledge(ctx: &Ctx, id: &str, body: &Value) -> (u16, Value) {
    let id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let token = match body["capability_token"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return err(400, "capability_token_required", "the per-notification capability token is required to acknowledge"),
    };
    let conn = ctx.conn.lock().unwrap();
    let resolution: Option<String> = conn
        .query_row("SELECT resolution FROM event WHERE id=?1", [id], |r| r.get(0))
        .optional()
        .unwrap_or(None);
    match resolution.as_deref() {
        None => return err(404, "event_not_found", "no such event"),
        Some("firing") => {}
        Some(_) => return err(409, "event_not_firing", "event is not firing"),
    }
    if !auth::verify_cap_token(&conn, id, token) {
        warn!("acknowledge rejected for event {id}: bad/used capability token");
        return err(403, "capability_token_invalid", "capability token invalid or already used");
    }
    let now = timeutil::now();
    let n = conn
        .execute(
            // single-use: flip used flag in the same statement that resolves.
            "UPDATE event SET resolution='acknowledged', resolved_at=?1, cap_token_used=1
             WHERE id=?2 AND resolution='firing' AND cap_token_used=0",
            params![now, id],
        )
        .unwrap_or(0);
    if n == 0 {
        return err(409, "capability_token_invalid", "token already consumed");
    }
    info!("event {id} acknowledged — escalation stops");
    (200, event_json(&conn, id).unwrap_or(json!({"id": id, "resolution": "acknowledged"})))
}

// ---------------------------------------------------------------- notifiers

fn notifier_register(ctx: &Ctx, body: &Value) -> (u16, Value) {
    let label = match body["label"].as_str() {
        Some(l) if !l.is_empty() => l,
        _ => return err(400, "missing_label", "label is required"),
    };
    let tiers: Vec<String> = body["tiers"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if tiers.is_empty() {
        return err(400, "missing_tiers", "tiers must be a non-empty array");
    }
    for t in &tiers {
        if !["alarm", "degraded", "heartbeat"].contains(&t.as_str()) {
            return err(400, "unknown_tier", "tiers must be among alarm/degraded/heartbeat");
        }
    }
    // ack_window_secs: null is a DELIBERATE fire-and-forget declaration
    // (invariant 10) — only an explicit null in the body means null; an
    // absent field gets the default window.
    let conn = ctx.conn.lock().unwrap();
    let ack_window = if body.get("ack_window_secs").map(|v| v.is_null()).unwrap_or(false) {
        None
    } else {
        Some(
            body["ack_window_secs"]
                .as_i64()
                .unwrap_or_else(|| db::config_get_i64(&conn, "notifier_default_ack_window_secs")),
        )
    };
    match outbox::register_notifier(&conn, label, &tiers, ack_window) {
        Ok(id) => (201, json!({"id": id, "label": label, "tiers": tiers, "ack_window_secs": ack_window})),
        Err(e) => err(500, "db_error", &e.to_string()),
    }
}

/// Long-poll pull (§11.1): held open until a pending delivery exists for THIS
/// notifier, or the window lapses. Identity comes from ?label=… — the shared
/// notifier-scope secret authenticates the CLASS; the label selects whose
/// delivery rows. Fan-out, non-exclusive.
/// Pull a single ?key=value query parameter out of a raw URL.
fn query_param(url: &str, key: &str) -> Option<String> {
    url.split('?').nth(1).and_then(|q| {
        q.split('&').find_map(|kv| {
            kv.split_once('=')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
    })
}

/// Control-scope display of recent outbox messages (§11.1: control may READ
/// the outbox, never ack it). Payloads are returned parsed; this is the one
/// sanctioned place a capability token is visible to the operator — by
/// design, so a lost push notification doesn't strand an un-ackable alarm.
fn outbox_list(ctx: &Ctx, url: &str) -> (u16, Value) {
    let tier = query_param(url, "tier");
    let conn = ctx.conn.lock().unwrap();
    let mut sql = String::from(
        "SELECT id, tier, kind, wallet_id, event_id, payload, created_at
         FROM outbox",
    );
    if tier.is_some() {
        sql.push_str(" WHERE tier=?1");
    }
    sql.push_str(" ORDER BY id DESC LIMIT 100");
    let mut stmt = conn.prepare(&sql).unwrap();
    let map = |r: &rusqlite::Row| -> rusqlite::Result<Value> {
        let payload: String = r.get(5)?;
        let created: i64 = r.get(6)?;
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "tier": r.get::<_, String>(1)?,
            "kind": r.get::<_, String>(2)?,
            "wallet_id": r.get::<_, Option<i64>>(3)?,
            "event_id": r.get::<_, Option<i64>>(4)?,
            "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
            "created_at": timeutil::iso8601(created),
        }))
    };
    let rows: Vec<Value> = if let Some(t) = tier {
        stmt.query_map([t], map).unwrap().filter_map(|r| r.ok()).collect()
    } else {
        stmt.query_map([], map).unwrap().filter_map(|r| r.ok()).collect()
    };
    (200, json!(rows))
}

fn outbox_pending(ctx: &Ctx, url: &str) -> (u16, Value) {
    let label = url
        .split('?')
        .nth(1)
        .and_then(|q| {
            q.split('&').find_map(|kv| kv.strip_prefix("label=").map(str::to_string))
        });
    let label = match label {
        Some(l) => l,
        None => return err(400, "missing_label", "?label=<notifier label> is required"),
    };
    let longpoll = {
        let conn = ctx.conn.lock().unwrap();
        db::config_get_i64(&conn, "outbox_longpoll_secs").max(1) as u64
    };
    let deadline = Instant::now() + Duration::from_secs(longpoll);
    loop {
        {
            let conn = ctx.conn.lock().unwrap();
            let nid: Option<i64> = conn
                .query_row("SELECT id FROM notifier WHERE label=?1", [label.as_str()], |r| r.get(0))
                .optional()
                .unwrap_or(None);
            let nid = match nid {
                Some(n) => n,
                None => return err(404, "notifier_not_registered", "register first"),
            };
            let _ = conn.execute(
                "UPDATE notifier SET last_seen_at=?1 WHERE id=?2",
                params![timeutil::now(), nid],
            );
            match outbox::pending_for(&conn, nid) {
                Ok(rows) if !rows.is_empty() => return (200, json!({"deliveries": rows})),
                Ok(_) => {}
                Err(e) => return err(500, "db_error", &e.to_string()),
            }
        } // mutex released before sleeping — never hold the DB while idle
        if Instant::now() >= deadline {
            return (200, json!({"deliveries": []}));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn outbox_ack(ctx: &Ctx, id: &str, body: &Value, stage: &str) -> (u16, Value) {
    let delivery_id = match parse_id(id) { Ok(i) => i, Err(e) => return e };
    let label = match body["label"].as_str() {
        Some(l) => l,
        None => return err(400, "missing_label", "label is required"),
    };
    let conn = ctx.conn.lock().unwrap();
    let nid: Option<i64> = conn
        .query_row("SELECT id FROM notifier WHERE label=?1", [label], |r| r.get(0))
        .optional()
        .unwrap_or(None);
    let nid = match nid {
        Some(n) => n,
        None => return err(404, "notifier_not_registered", "register first"),
    };
    match outbox::ack(&conn, delivery_id, nid, stage) {
        Ok(true) => (200, json!({"delivery_id": delivery_id, "state": stage})),
        Ok(false) => err(409, "ack_out_of_order", "delivery not in the expected prior state for this ack"),
        Err(e) => err(500, "db_error", &e.to_string()),
    }
}
