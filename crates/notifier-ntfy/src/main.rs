//! satsurv-notifier-ntfy — the loud path.
//!
//! A separate deployable unit (architectural constraint): stateless, speaks
//! ONLY the published outbox contract (register → long-poll pull → deliver →
//! two-stage ack), and issues no verbs — its notifier-scope secret makes a
//! verb call physically impossible (403 by role).
//!
//! Alarm-tier messages carry the per-event capability token; we render an
//! ntfy HTTP action button so a tap on the phone acknowledges (full M7
//! security requires TLS in front of the core via `tailscale serve`).
//!
//! Environment:
//!   SATSURV_API       core base URL          (default http://127.0.0.1:8330)
//!   SATSURV_SECRET    notifier-scope secret, or SATSURV_SECRET_FILE (default ./notifier.secret)
//!   NTFY_URL          full topic URL, e.g. https://ntfy.sh/my-secret-topic (required)
//!   NTFY_TOKEN        optional ntfy auth token
//!   NOTIFIER_LABEL    identity label          (default "ntfy")
//!   NOTIFIER_TIERS    comma list              (default "alarm,degraded,heartbeat")
//!   PUBLIC_API_URL    URL the PHONE can reach the core at, for the ack
//!                     button (e.g. https://host.tailnet.ts.net). Unset → no button.

use serde_json::{json, Value};
use std::time::Duration;

struct Cfg {
    api: String,
    secret: String,
    ntfy_url: String,
    ntfy_token: Option<String>,
    label: String,
    tiers: Vec<String>,
    public_api: Option<String>,
}

fn cfg() -> Cfg {
    let secret = std::env::var("SATSURV_SECRET").ok().or_else(|| {
        let p = std::env::var("SATSURV_SECRET_FILE").unwrap_or_else(|_| "notifier.secret".into());
        std::fs::read_to_string(p).ok()
    });
    let ntfy_url = std::env::var("NTFY_URL").unwrap_or_default();
    if ntfy_url.is_empty() {
        eprintln!("NTFY_URL is required (e.g. https://ntfy.sh/your-secret-topic)");
        std::process::exit(1);
    }
    Cfg {
        api: std::env::var("SATSURV_API").unwrap_or_else(|_| "http://127.0.0.1:8330".into()),
        secret: secret.map(|s| s.trim().to_string()).unwrap_or_else(|| {
            eprintln!("no notifier secret: set SATSURV_SECRET or SATSURV_SECRET_FILE");
            std::process::exit(1)
        }),
        ntfy_url,
        ntfy_token: std::env::var("NTFY_TOKEN").ok(),
        label: std::env::var("NOTIFIER_LABEL").unwrap_or_else(|_| "ntfy".into()),
        tiers: std::env::var("NOTIFIER_TIERS")
            .unwrap_or_else(|_| "alarm,degraded,heartbeat".into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        public_api: std::env::var("PUBLIC_API_URL").ok().filter(|s| !s.is_empty()),
    }
}

fn core_post(c: &Cfg, path: &str, body: Value) -> Result<Value, String> {
    ureq::post(&format!("{}{path}", c.api))
        .set("Authorization", &format!("Bearer {}", c.secret))
        .send_json(body)
        .map_err(|e| e.to_string())
        .and_then(|r| r.into_json().map_err(|e| e.to_string()))
}

fn register(c: &Cfg) {
    // Idempotent: re-registering with the same label updates tiers/window.
    let body = json!({
        "label": c.label,
        "tiers": c.tiers,
        "ack_window_secs": 300,   // we PROMISE prompt delivery — delinquency applies (§9.3)
    });
    loop {
        match core_post(c, "/notifiers/register", body.clone()) {
            Ok(_) => {
                eprintln!("registered as '{}' for tiers {:?}", c.label, c.tiers);
                return;
            }
            Err(e) => {
                eprintln!("register failed ({e}) — core not up yet? retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

/// Map tier → ntfy priority. The alarm tier is maximally loud.
fn priority(tier: &str) -> &'static str {
    match tier {
        "alarm" => "urgent",
        "degraded" => "high",
        _ => "default",
    }
}

fn deliver(c: &Cfg, payload: &Value) -> Result<(), String> {
    let tier = payload["tier"].as_str().unwrap_or("heartbeat");
    let title = payload["title"].as_str().unwrap_or("Sat Surveillance");
    let body = payload["body"].as_str().unwrap_or("");
    let mut req = ureq::post(&c.ntfy_url)
        .set("Title", title)
        .set("Priority", priority(tier))
        .set(
            "Tags",
            match tier {
                "alarm" => "rotating_light",
                "degraded" => "warning",
                _ => "heartbeat",
            },
        );
    if let Some(tok) = &c.ntfy_token {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    // Phone-button ack: only when the core is reachable FROM THE PHONE
    // (PUBLIC_API_URL, ideally TLS via tailscale serve — design M7).
    if let (Some(base), Some(ack_path), Some(token)) = (
        &c.public_api,
        payload["ack_path"].as_str(),
        payload["capability_token"].as_str(),
    ) {
        let action = format!(
            "http, Acknowledge, {base}{ack_path}, method=POST, headers.Content-Type=application/json, body={}",
            json!({"capability_token": token})
        );
        req = req.set("Actions", &action);
    }
    req.send_string(body).map_err(|e| e.to_string())?;
    Ok(())
}

fn main() {
    let c = cfg();
    register(&c);
    let pull = format!("/outbox/pending?label={}", c.label);
    loop {
        let resp = ureq::get(&format!("{}{pull}", c.api))
            .set("Authorization", &format!("Bearer {}", c.secret))
            .timeout(Duration::from_secs(60)) // > server long-poll window
            .call();
        let v: Value = match resp.and_then(|r| {
            r.into_json()
                .map_err(|e| ureq::Error::from(std::io::Error::new(std::io::ErrorKind::Other, e)))
        }) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("pull failed: {e} — retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let deliveries = v["deliveries"].as_array().cloned().unwrap_or_default();
        for d in deliveries {
            let delivery_id = d["delivery_id"].as_i64().unwrap_or(-1);
            // Stage 1: "I have it, will deliver" — starts the ack clock.
            let _ = core_post(
                &c,
                &format!("/outbox/{delivery_id}/received"),
                json!({"label": c.label}),
            );
            match deliver(&c, &d["payload"]) {
                Ok(()) => {
                    // Stage 2: actually out the door.
                    let _ = core_post(
                        &c,
                        &format!("/outbox/{delivery_id}/delivered"),
                        json!({"label": c.label}),
                    );
                }
                Err(e) => {
                    // No delivered-ack ⇒ the row stays 'received' past our
                    // promised window ⇒ the core flags us delinquent BY NAME.
                    // That's correct: honesty over papering-over (inv. 16).
                    eprintln!("ntfy delivery {delivery_id} failed: {e}");
                }
            }
        }
    }
}
