//! Authentication (design §10.2).
//!
//! Two role-scoped secrets, generated at `init`:
//!   * control-scope  — verbs + queries + outbox *read* (CLI, web UI)
//!   * notifier-scope — register / pull / ack ONLY
//! Different secrets, enforced at the credential: a notifier presenting its
//! secret to a verb endpoint fails 403 at the door (invariant 12).
//!
//! Only SHA-256 hashes of the secrets live in the DB; the plaintext is
//! written once to files beside the DB (0600) for the matching clients.
//!
//! Capability token: HMAC-SHA256(event_id, token_key), hex. Single-use,
//! scoped to one event. Invariant 11: triggering is auth-free and loud;
//! silencing requires this token on top of the control-scope secret.

use hmac::{Hmac, Mac};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::db;

type HmacSha256 = Hmac<Sha256>;

pub const META_CONTROL_HASH: &str = "control_secret_hash";
pub const META_NOTIFIER_HASH: &str = "notifier_secret_hash";
pub const META_TOKEN_KEY: &str = "cap_token_key";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Control,
    Notifier,
}

fn random_hex32() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(hex::encode(buf))
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Generate both role secrets + the token key. Writes plaintext secret files
/// next to the DB, stores only hashes (and the token key, which never leaves
/// the daemon) in `meta`. Idempotent: refuses to overwrite existing secrets
/// unless `rotate`.
pub fn init_secrets(conn: &Connection, db_path: &Path, rotate: bool) -> std::io::Result<()> {
    if !rotate && db::meta_get(conn, META_CONTROL_HASH).is_some() {
        return Ok(());
    }
    let dir = db_path.parent().unwrap_or(Path::new("."));
    let control = random_hex32()?;
    let notifier = random_hex32()?;
    let token_key = random_hex32()?;

    for (name, secret) in [
        ("control.secret", &control),
        ("notifier.secret", &notifier),
    ] {
        let p = dir.join(name);
        fs::write(&p, secret)?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600))?;
    }
    db::meta_set(conn, META_CONTROL_HASH, &sha256_hex(&control)).map_err(io_err)?;
    db::meta_set(conn, META_NOTIFIER_HASH, &sha256_hex(&notifier)).map_err(io_err)?;
    db::meta_set(conn, META_TOKEN_KEY, &token_key).map_err(io_err)?;
    Ok(())
}

fn io_err(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e)
}

/// Which role (if any) does this bearer secret authenticate as?
pub fn role_for_secret(conn: &Connection, secret: &str) -> Option<Role> {
    let h = sha256_hex(secret);
    if db::meta_get(conn, META_CONTROL_HASH).as_deref() == Some(h.as_str()) {
        return Some(Role::Control);
    }
    if db::meta_get(conn, META_NOTIFIER_HASH).as_deref() == Some(h.as_str()) {
        return Some(Role::Notifier);
    }
    None
}

/// Mint the single-use capability token for an event (sign on the way out).
pub fn mint_cap_token(conn: &Connection, event_id: i64) -> Option<String> {
    let key = db::meta_get(conn, META_TOKEN_KEY)?;
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).ok()?;
    mac.update(event_id.to_string().as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

/// Verify on the way back. Constant-time-ish via hash compare of hex strings
/// is fine here (token is high-entropy HMAC output; timing leaks nothing
/// useful), but we compare hashes to keep the stored form non-replayable
/// even if the DB leaks.
pub fn verify_cap_token(conn: &Connection, event_id: i64, presented: &str) -> bool {
    match mint_cap_token(conn, event_id) {
        Some(expected) => {
            // store/compare as hash of the token (DB holds cap_token_hash)
            sha256_hex(&expected) == sha256_hex(presented)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        db::init_schema(&c).unwrap();
        db::meta_set(&c, META_TOKEN_KEY, "test-key").unwrap();
        c
    }

    #[test]
    fn token_roundtrip_and_scoping() {
        let c = mem();
        let t1 = mint_cap_token(&c, 1).unwrap();
        let t2 = mint_cap_token(&c, 2).unwrap();
        assert_ne!(t1, t2, "token must be scoped to the event");
        assert!(verify_cap_token(&c, 1, &t1));
        assert!(!verify_cap_token(&c, 1, &t2), "token for another event must not dismiss this one");
        assert!(!verify_cap_token(&c, 1, "garbage"));
    }

    #[test]
    fn role_resolution() {
        let c = mem();
        db::meta_set(&c, META_CONTROL_HASH, &sha256_hex("ctl")).unwrap();
        db::meta_set(&c, META_NOTIFIER_HASH, &sha256_hex("ntf")).unwrap();
        assert_eq!(role_for_secret(&c, "ctl"), Some(Role::Control));
        assert_eq!(role_for_secret(&c, "ntf"), Some(Role::Notifier));
        assert_eq!(role_for_secret(&c, "nope"), None);
    }
}
