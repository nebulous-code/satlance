//! satsurv-notifier-smtp — the escalation/backup channel.
//!
//! Same shape as the ntfy notifier: stateless outbox client, no verbs, no
//! state. Delivery is a minimal hand-rolled SMTP conversation over TCP.
//!
//! Deliberate scope: PLAIN SMTP (optionally with AUTH LOGIN) — no STARTTLS.
//! Point it at a localhost relay (msmtp, postfix satellite, or the
//! mailhog/smtp4dev test server in the regtest compose); the relay owns TLS
//! to the real world. Keeping TLS out keeps this unit tiny and auditable,
//! and avoids shipping credentials over the wire to remote hosts.
//!
//! Environment:
//!   SATSURV_API / SATSURV_SECRET / SATSURV_SECRET_FILE   (as ntfy notifier)
//!   SMTP_HOST (default 127.0.0.1)   SMTP_PORT (default 25)
//!   SMTP_USER / SMTP_PASS           optional AUTH LOGIN (base64'd here)
//!   SMTP_FROM (required)            SMTP_TO (required)
//!   NOTIFIER_LABEL (default "smtp") NOTIFIER_TIERS (default "alarm,degraded")

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

struct Cfg {
    api: String,
    secret: String,
    host: String,
    port: u16,
    user: Option<String>,
    pass: Option<String>,
    from: String,
    to: String,
    label: String,
    tiers: Vec<String>,
}

fn cfg() -> Cfg {
    let secret = std::env::var("SATSURV_SECRET").ok().or_else(|| {
        let p = std::env::var("SATSURV_SECRET_FILE").unwrap_or_else(|_| "notifier.secret".into());
        std::fs::read_to_string(p).ok()
    });
    let req = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| {
            eprintln!("{k} is required");
            std::process::exit(1)
        })
    };
    Cfg {
        api: std::env::var("SATSURV_API").unwrap_or_else(|_| "http://127.0.0.1:8330".into()),
        secret: secret.map(|s| s.trim().to_string()).unwrap_or_else(|| {
            eprintln!("no notifier secret: set SATSURV_SECRET or SATSURV_SECRET_FILE");
            std::process::exit(1)
        }),
        host: std::env::var("SMTP_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("SMTP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(25),
        user: std::env::var("SMTP_USER").ok().filter(|s| !s.is_empty()),
        pass: std::env::var("SMTP_PASS").ok().filter(|s| !s.is_empty()),
        from: req("SMTP_FROM"),
        to: req("SMTP_TO"),
        label: std::env::var("NOTIFIER_LABEL").unwrap_or_else(|_| "smtp".into()),
        tiers: std::env::var("NOTIFIER_TIERS")
            .unwrap_or_else(|_| "alarm,degraded".into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    }
}

// -- tiny base64 (same table the core uses for cookie auth) -----------------
fn b64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// -- minimal SMTP conversation ----------------------------------------------
struct Smtp {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Smtp {
    fn connect(host: &str, port: u16) -> Result<Self, String> {
        let stream = TcpStream::connect((host, port)).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        let mut s = Smtp { reader: BufReader::new(stream), writer };
        s.expect(2)?; // greeting 220
        Ok(s)
    }

    fn cmd(&mut self, line: &str, expect_class: u8) -> Result<(), String> {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .map_err(|e| e.to_string())?;
        self.expect(expect_class)
    }

    /// Read a (possibly multi-line) reply; require the status class (2xx/3xx).
    fn expect(&mut self, class: u8) -> Result<(), String> {
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).map_err(|e| e.to_string())?;
            if line.len() < 4 {
                return Err(format!("short SMTP reply: {line:?}"));
            }
            let got = line.as_bytes()[0] - b'0';
            let more = line.as_bytes()[3] == b'-';
            if !more {
                return if got == class {
                    Ok(())
                } else {
                    Err(format!("SMTP error: {}", line.trim_end()))
                };
            }
        }
    }
}

fn send_mail(c: &Cfg, subject: &str, body: &str) -> Result<(), String> {
    let mut s = Smtp::connect(&c.host, c.port)?;
    s.cmd("EHLO satsurv", 2)?;
    if let (Some(u), Some(p)) = (&c.user, &c.pass) {
        s.cmd("AUTH LOGIN", 3)?;
        s.cmd(&b64(u.as_bytes()), 3)?;
        s.cmd(&b64(p.as_bytes()), 2)?;
    }
    s.cmd(&format!("MAIL FROM:<{}>", c.from), 2)?;
    s.cmd(&format!("RCPT TO:<{}>", c.to), 2)?;
    s.cmd("DATA", 3)?;
    // Dot-stuff body lines per RFC 5321 §4.5.2.
    let stuffed: String = body
        .lines()
        .map(|l| if l.starts_with('.') { format!(".{l}") } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\r\n");
    let msg = format!(
        "From: Sat Surveillance <{}>\r\nTo: <{}>\r\nSubject: {}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n.",
        c.from, c.to, subject, stuffed
    );
    s.cmd(&msg, 2)?;
    s.cmd("QUIT", 2).ok();
    Ok(())
}

// -- outbox client (identical shape to the ntfy unit) ------------------------
fn core_post(c: &Cfg, path: &str, body: Value) -> Result<Value, String> {
    ureq::post(&format!("{}{path}", c.api))
        .set("Authorization", &format!("Bearer {}", c.secret))
        .send_json(body)
        .map_err(|e| e.to_string())
        .and_then(|r| r.into_json().map_err(|e| e.to_string()))
}

fn main() {
    let c = cfg();
    let reg = json!({"label": c.label, "tiers": c.tiers, "ack_window_secs": 300});
    loop {
        match core_post(&c, "/notifiers/register", reg.clone()) {
            Ok(_) => break,
            Err(e) => {
                eprintln!("register failed ({e}) — retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
    eprintln!("registered as '{}' for tiers {:?}", c.label, c.tiers);

    let pull = format!("/outbox/pending?label={}", c.label);
    loop {
        let resp = ureq::get(&format!("{}{pull}", c.api))
            .set("Authorization", &format!("Bearer {}", c.secret))
            .timeout(Duration::from_secs(60))
            .call();
        let v: Value = match resp {
            Ok(r) => r.into_json().unwrap_or(Value::Null),
            Err(e) => {
                eprintln!("pull failed: {e} — retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        for d in v["deliveries"].as_array().cloned().unwrap_or_default() {
            let delivery_id = d["delivery_id"].as_i64().unwrap_or(-1);
            let _ = core_post(&c, &format!("/outbox/{delivery_id}/received"), json!({"label": c.label}));
            let p = &d["payload"];
            let subject = p["title"].as_str().unwrap_or("Sat Surveillance");
            let mut body = p["body"].as_str().unwrap_or("").to_string();
            // Email can't render a button; include the ack command instead.
            if let (Some(eid), Some(tok)) = (p["event_id"].as_i64(), p["capability_token"].as_str()) {
                body.push_str(&format!("\n\nTo acknowledge: satsurv event ack {eid} {tok}\n"));
            }
            match send_mail(&c, subject, &body) {
                Ok(()) => {
                    let _ = core_post(&c, &format!("/outbox/{delivery_id}/delivered"), json!({"label": c.label}));
                }
                Err(e) => eprintln!("smtp delivery {delivery_id} failed: {e}"),
            }
        }
    }
}
