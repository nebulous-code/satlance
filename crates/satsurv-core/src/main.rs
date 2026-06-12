//! satsurvd — the Sat Surveillance core daemon.
//!
//! Subcommands:
//!   satsurvd init   verify node (RPC reachable, past IBD, ZMQ publishing),
//!                   create schema + secrets, write node config to SQLite.
//!                   CLI-only by design (§11.1): it bootstraps the thing
//!                   that serves HTTP, so it can't be served by it.
//!   satsurvd run    start the worker, ZMQ subscriber, and HTTP API.
//!
//! Environment:
//!   SATSURV_DB          path to the SQLite DB    (default ./satsurv.db)
//!   SATSURV_RPC_URL     node RPC base URL        (init only; persisted)
//!   SATSURV_RPC_COOKIE  path to the .cookie file (init only; persisted)
//!   SATSURV_BIND        API bind address         (default 127.0.0.1:8330 —
//!                       never bind 0.0.0.0; use `tailscale serve` for remote)

mod api;
mod arming;
mod auth;
mod db;
mod degraded;
mod detect;
mod outbox;
mod reconcile;
mod rpc;
mod sats;
mod timeutil;
mod worker;
mod zmqsub;

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use log::{error, info};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("init") => cmd_init(),
        Some("run") => cmd_run(),
        Some("rotate-secrets") => cmd_rotate_secrets(),
        _ => {
            eprintln!("usage: satsurvd <init|run|rotate-secrets>   (see module docs for env vars)");
            2
        }
    };
    std::process::exit(code);
}

/// rotate-secrets: regenerate both role secrets (and the cap-token key) on an
/// existing DB. Old credentials stop working immediately; restart notifiers
/// with the new notifier.secret. Run while the daemon is stopped.
fn cmd_rotate_secrets() -> i32 {
    let db_path = PathBuf::from(env_or("SATSURV_DB", "satsurv.db"));
    if !db_path.exists() {
        error!("no DB at {} — run init first", db_path.display());
        return 1;
    }
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            error!("cannot open DB: {e}");
            return 1;
        }
    };
    match auth::init_secrets(&conn, &db_path, true) {
        Ok(()) => {
            info!(
                "secrets rotated: new control.secret / notifier.secret written beside {} (0600). \
                 All previously issued credentials are now invalid.",
                db_path.display()
            );
            0
        }
        Err(e) => {
            error!("rotation failed: {e}");
            1
        }
    }
}

/// init: every check names its failure clearly (M0 done-when) and refuses —
/// a guard that starts half-connected is worse than one that won't start.
fn cmd_init() -> i32 {
    let db_path = PathBuf::from(env_or("SATSURV_DB", "satsurv.db"));
    let rpc_url = env_or("SATSURV_RPC_URL", "http://127.0.0.1:8332");
    let cookie = env_or("SATSURV_RPC_COOKIE", "");
    if cookie.is_empty() {
        error!("init refused: SATSURV_RPC_COOKIE is not set (path to the node's .cookie file)");
        return 1;
    }

    let rpc = rpc::RpcClient::new(&rpc_url, &cookie);

    // Check 1: RPC reachable.
    let info = match rpc.get_blockchain_info() {
        Ok(i) => i,
        Err(e) => {
            error!("init refused: node RPC unreachable at {rpc_url}: {e}");
            return 1;
        }
    };
    let chain = info["chain"].as_str().unwrap_or("unknown").to_string();
    info!("node reachable: chain={chain}, blocks={}", info["blocks"]);

    // Check 2: past IBD (regtest exempt — it is born synced).
    let ibd = info["initialblockdownload"].as_bool().unwrap_or(true);
    if ibd && chain != "regtest" {
        error!(
            "init refused: node is in initial block download ({} of {} headers). \
             Wallet state is not trustworthy until sync completes — retry after IBD.",
            info["blocks"], info["headers"]
        );
        return 1;
    }

    // Check 3: ZMQ publishing.
    let zmq = match rpc.get_zmq_notifications() {
        Ok(z) => z,
        Err(e) => {
            error!("init refused: getzmqnotifications failed: {e}. Enable ZMQ in the node config (e.g. zmqpubrawtx/zmqpubhashblock).");
            return 1;
        }
    };
    let endpoints: Vec<String> = zmq
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| n["address"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if endpoints.is_empty() {
        error!("init refused: node reports NO ZMQ notification endpoints. Add zmqpubrawtx=tcp://0.0.0.0:28332 (and hashblock) to the node config and restart it.");
        return 1;
    }
    info!("ZMQ confirmed publishing: {} endpoint(s)", endpoints.len());

    // All checks passed → persist.
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            error!("init refused: cannot open/create DB at {}: {e}", db_path.display());
            return 1;
        }
    };
    if let Err(e) = db::init_schema(&conn).and_then(|_| db::config_seed_defaults(&conn)) {
        error!("init refused: schema creation failed: {e}");
        return 1;
    }
    let _ = db::meta_set(&conn, "rpc_url", &rpc_url);
    let _ = db::meta_set(&conn, "rpc_cookie_path", &cookie);
    let _ = db::meta_set(&conn, "network", &chain);
    let _ = db::meta_set(&conn, "zmq_endpoints", &serde_json::to_string(&endpoints).unwrap_or_default());
    if let Err(e) = auth::init_secrets(&conn, &db_path, false) {
        error!("init refused: secret generation failed: {e}");
        return 1;
    }
    info!(
        "init complete: DB at {}, secrets written beside it (control.secret / notifier.secret, 0600). \
         Start with `satsurvd run`.",
        db_path.display()
    );
    0
}

fn cmd_run() -> i32 {
    let db_path = PathBuf::from(env_or("SATSURV_DB", "satsurv.db"));
    if !db_path.exists() {
        error!("no DB at {} — run `satsurvd init` first", db_path.display());
        return 1;
    }
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            error!("cannot open DB: {e}");
            return 1;
        }
    };
    // Schema is idempotent; re-applying covers upgrades that add tables.
    let _ = db::init_schema(&conn);
    let _ = db::config_seed_defaults(&conn);

    let rpc_url = match db::meta_get(&conn, "rpc_url") {
        Some(u) => u,
        None => {
            error!("DB has no node config — run `satsurvd init` first");
            return 1;
        }
    };
    let cookie = db::meta_get(&conn, "rpc_cookie_path").unwrap_or_default();
    let network = db::meta_get(&conn, "network").unwrap_or_else(|| "unknown".into());
    let zmq_endpoints: Vec<String> = db::meta_get(&conn, "zmq_endpoints")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let bind = env_or("SATSURV_BIND", "127.0.0.1:8330");
    if bind.starts_with("0.0.0.0") && env_or("SATSURV_ALLOW_NONLOOPBACK", "0") != "1" {
        // §11: never expose to the LAN/world; remote access is tailscale serve's
        // job. SATSURV_ALLOW_NONLOOPBACK=1 exists only for container networks
        // (compose service-to-service), where "0.0.0.0" is still namespace-private.
        error!("refusing to bind 0.0.0.0 — bind localhost and use `tailscale serve` for remote access (set SATSURV_ALLOW_NONLOOPBACK=1 only inside an isolated container network)");
        return 1;
    }

    info!("satsurvd {VERSION} starting: network={network}, db={}", db_path.display());

    let conn = Arc::new(Mutex::new(conn));
    let (tx, rx) = mpsc::channel::<worker::Trigger>();

    // ZMQ subscriber (trigger path).
    zmqsub::spawn(zmq_endpoints, tx.clone());

    // Worker (the single owner of node-facing + timed work).
    {
        let conn = Arc::clone(&conn);
        let rpc = rpc::RpcClient::new(&rpc_url, &cookie);
        let network = network.clone();
        std::thread::Builder::new()
            .name("worker".into())
            .spawn(move || worker::run(conn, rpc, network, rx))
            .expect("spawn worker");
    }

    // HTTP API (blocks this thread).
    let ctx = Arc::new(api::Ctx {
        conn,
        rpc: rpc::RpcClient::new(&rpc_url, &cookie),
        nudge: tx,
        version: VERSION,
    });
    if let Err(e) = api::serve(&bind, ctx) {
        error!("api server failed: {e}");
        return 1;
    }
    0
}
