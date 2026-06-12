//! satsurv — the CLI control surface.
//!
//! A separate deployable unit (architectural constraint): it speaks ONLY the
//! published control contract over HTTP and holds no state. Anything the CLI
//! can do, any other control surface (web UI, script) can do identically.
//!
//! Environment:
//!   SATSURV_API     base URL (default http://127.0.0.1:8330)
//!   SATSURV_SECRET  control-scope secret, or
//!   SATSURV_SECRET_FILE path to it (default ./control.secret)

use serde_json::{json, Value};

fn base() -> String {
    std::env::var("SATSURV_API").unwrap_or_else(|_| "http://127.0.0.1:8330".into())
}

fn secret() -> String {
    if let Ok(s) = std::env::var("SATSURV_SECRET") {
        return s.trim().to_string();
    }
    let path = std::env::var("SATSURV_SECRET_FILE").unwrap_or_else(|_| "control.secret".into());
    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            eprintln!("no control secret: set SATSURV_SECRET or SATSURV_SECRET_FILE (tried '{path}')");
            std::process::exit(1);
        }
    }
}

fn call(method: &str, path: &str, body: Option<Value>) -> Value {
    let url = format!("{}{}", base(), path);
    let auth = format!("Bearer {}", secret());
    let req = match method {
        "GET" => ureq::get(&url),
        "POST" => ureq::post(&url),
        "DELETE" => ureq::delete(&url),
        _ => unreachable!(),
    }
    .set("Authorization", &auth);
    let resp = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match resp {
        Ok(r) => r.into_json().unwrap_or(Value::Null),
        Err(ureq::Error::Status(code, r)) => {
            let v: Value = r.into_json().unwrap_or(Value::Null);
            let (ec, em) = (
                v["error"]["code"].as_str().unwrap_or("unknown"),
                v["error"]["message"].as_str().unwrap_or(""),
            );
            eprintln!("error {code} [{ec}]: {em}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("cannot reach the daemon at {}: {e}", base());
            std::process::exit(1);
        }
    }
}

fn pretty(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn fmt_btc(sat: i64) -> String {
    let sign = if sat < 0 { "-" } else { "" };
    let a = sat.unsigned_abs();
    format!("{sign}{}.{:08}", a / 100_000_000, a % 100_000_000)
}

const USAGE: &str = "usage: satsurv <command>

  status                                       daemon + wallet + degraded overview
  wallet add <label> [--core-ref R] [--descriptor D]
  wallet list | show <id> | rm <id>
  wallet import <id> <descriptor>
  wallet arm <id> | disarm <id> | rescan <id>
  balance confirm <wallet_id> --observed-at <unix>   bless the observed balance
  balance dispute <wallet_id>                        'no, keep scanning'
  preauth add <wallet_id> <txid> <vout> [--note N] [--ttl SECS]
  preauth list | cancel <id>
  event list [--firing] | show <id>
  event ack <id> <capability_token>            silence a firing alarm

env: SATSURV_API, SATSURV_SECRET / SATSURV_SECRET_FILE";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["status"] => status(),

        ["wallet", "add", label, rest @ ..] => {
            let mut body = json!({"label": label});
            let mut it = rest.iter();
            while let Some(flag) = it.next() {
                match (*flag, it.next()) {
                    ("--core-ref", Some(v)) => body["core_wallet_ref"] = json!(v),
                    ("--descriptor", Some(v)) => body["descriptor"] = json!(v),
                    _ => die_usage(),
                }
            }
            pretty(&call("POST", "/wallets", Some(body)));
        }
        ["wallet", "list"] => pretty(&call("GET", "/wallets", None)),
        ["wallet", "show", id] => pretty(&call("GET", &format!("/wallets/{id}"), None)),
        ["wallet", "rm", id] => pretty(&call("DELETE", &format!("/wallets/{id}"), None)),
        ["wallet", "import", id, desc] => pretty(&call(
            "POST",
            &format!("/wallets/{id}/import"),
            Some(json!({"descriptor": desc})),
        )),
        ["wallet", "arm", id] => pretty(&call("POST", &format!("/wallets/{id}/arm"), Some(json!({})))),
        ["wallet", "disarm", id] => pretty(&call("POST", &format!("/wallets/{id}/disarm"), Some(json!({})))),
        ["wallet", "rescan", id] => pretty(&call("POST", &format!("/wallets/{id}/rescan"), Some(json!({})))),

        ["balance", "confirm", id, "--observed-at", at] => {
            let at: i64 = at.parse().unwrap_or_else(|_| {
                eprintln!("--observed-at must be the unix timestamp from the prompt / wallet show");
                std::process::exit(1)
            });
            pretty(&call(
                "POST",
                &format!("/wallets/{id}/balance/confirm"),
                Some(json!({"observed_at_unix": at})),
            ));
        }
        ["balance", "dispute", id] => pretty(&call(
            "POST",
            &format!("/wallets/{id}/balance/dispute"),
            Some(json!({})),
        )),

        ["preauth", "add", wallet_id, txid, vout, rest @ ..] => {
            let mut body = json!({
                "wallet_id": wallet_id.parse::<i64>().unwrap_or(-1),
                "txid": txid,
                "vout": vout.parse::<i64>().unwrap_or(-1),
            });
            let mut it = rest.iter();
            while let Some(flag) = it.next() {
                match (*flag, it.next()) {
                    ("--note", Some(v)) => body["note"] = json!(v),
                    ("--ttl", Some(v)) => body["ttl"] = json!(v.parse::<i64>().unwrap_or(86400)),
                    _ => die_usage(),
                }
            }
            pretty(&call("POST", "/preauths", Some(body)));
        }
        ["preauth", "list"] => pretty(&call("GET", "/preauths", None)),
        ["preauth", "cancel", id] => pretty(&call("DELETE", &format!("/preauths/{id}"), None)),

        ["event", "list"] => pretty(&call("GET", "/events", None)),
        ["event", "list", "--firing"] => pretty(&call("GET", "/events?resolution=firing", None)),
        ["event", "show", id] => pretty(&call("GET", &format!("/events/{id}"), None)),
        ["event", "ack", id, token] => pretty(&call(
            "POST",
            &format!("/events/{id}/acknowledge"),
            Some(json!({"capability_token": token})),
        )),

        _ => die_usage(),
    }
}

fn die_usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

/// Human-shaped status: the loud NOT PROTECTED section first (invariant 9).
fn status() {
    let v = call("GET", "/status", None);
    println!("Sat Surveillance {}", v["version"].as_str().unwrap_or("?"));
    let node = &v["node"];
    if node["reachable"].as_bool() == Some(true) {
        println!(
            "node: reachable ({} @ block {})",
            node["chain"].as_str().unwrap_or("?"),
            node["blocks"]
        );
    } else {
        println!("node: UNREACHABLE — {}", node["error"].as_str().unwrap_or(""));
    }
    let degraded = v["degraded"].as_array().cloned().unwrap_or_default();
    if degraded.is_empty() {
        println!("conditions: none");
    } else {
        println!("conditions: DEGRADED");
        for c in &degraded {
            println!("  ! {} — {}", c["key"].as_str().unwrap_or(""), c["detail"].as_str().unwrap_or(""));
        }
    }
    let np = v["not_protected"].as_array().cloned().unwrap_or_default();
    if !np.is_empty() {
        println!("\nNOT PROTECTED:");
        for l in &np {
            println!("  {}", l.as_str().unwrap_or(""));
        }
    }
    let armed = v["armed"].as_array().cloned().unwrap_or_default();
    if !armed.is_empty() {
        println!("\narmed:");
        for l in &armed {
            println!("  {}", l.as_str().unwrap_or(""));
        }
    }
    if let Some(f) = v["firing_events"].as_i64() {
        if f > 0 {
            println!("\n{f} alarm(s) FIRING — `satsurv event list --firing`");
        }
    }
    let uncovered = v["uncovered_tiers"].as_array().cloned().unwrap_or_default();
    if !uncovered.is_empty() {
        let names: Vec<&str> = uncovered.iter().filter_map(|t| t.as_str()).collect();
        println!("\nWARNING: no notifier registered for tier(s): {}", names.join(", "));
    }
    // keep fmt_btc referenced for future table output
    let _ = fmt_btc(0);
}
