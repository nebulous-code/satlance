//! JSON-RPC client to Bitcoin Core/Knots. Cookie auth, same-host assumption
//! (design §4). RPC is the ask/confirm path; ZMQ is only the trigger
//! (invariant 4). All amounts crossing this boundary convert to integer
//! satoshis immediately (invariant 17).

use serde_json::{json, Value};
use std::fmt;
use std::fs;
use std::time::Duration;

#[derive(Debug)]
pub enum RpcError {
    /// Network / transport / node-down class — drives `blinded` (§7.1).
    Transport(String),
    /// The node answered with a JSON-RPC error object.
    Node { code: i64, message: String },
    /// Malformed response.
    Protocol(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Transport(e) => write!(f, "node unreachable: {e}"),
            RpcError::Node { code, message } => write!(f, "node rpc error {code}: {message}"),
            RpcError::Protocol(e) => write!(f, "rpc protocol error: {e}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, RpcError>;

#[derive(Clone)]
pub struct RpcClient {
    url: String,
    cookie_path: String,
    agent: ureq::Agent,
}

fn b64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
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

impl RpcClient {
    pub fn new(url: &str, cookie_path: &str) -> Self {
        RpcClient {
            url: url.trim_end_matches('/').to_string(),
            cookie_path: cookie_path.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(120)) // rescans can stall wallet RPC (§7)
                .build(),
        }
    }

    /// Cookie is re-read per call: Core rewrites it on every node restart.
    fn auth_header(&self) -> Result<String> {
        let cookie = fs::read_to_string(&self.cookie_path)
            .map_err(|e| RpcError::Transport(format!("cookie file: {e}")))?;
        Ok(format!("Basic {}", b64(cookie.trim().as_bytes())))
    }

    pub fn call(&self, wallet: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_with_timeout(wallet, method, params, None)
    }

    /// Like `call`, but with a per-request timeout override. Used for RPCs
    /// that legitimately outlive any sane wait (a full-history rescan inside
    /// `importdescriptors` runs for hours on mainnet): a short timeout there
    /// means "the node took the job", not failure — the caller polls
    /// `getwalletinfo.scanning` for completion instead of holding the line.
    pub fn call_with_timeout(
        &self,
        wallet: Option<&str>,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let url = match wallet {
            Some(w) => format!("{}/wallet/{}", self.url, w),
            None => self.url.clone(),
        };
        let body = json!({"jsonrpc": "1.0", "id": "satlance", "method": method, "params": params});
        let mut req = self
            .agent
            .post(&url)
            .set("Authorization", &self.auth_header()?);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req.send_json(body);
        let v: Value = match resp {
            Ok(r) => r
                .into_json()
                .map_err(|e| RpcError::Protocol(e.to_string()))?,
            // ureq returns Err for HTTP >= 400, but bitcoind sends RPC errors
            // with 500 — recover the body in that case.
            Err(ureq::Error::Status(_, r)) => r
                .into_json()
                .map_err(|e| RpcError::Protocol(e.to_string()))?,
            Err(ureq::Error::Transport(t)) => return Err(RpcError::Transport(t.to_string())),
        };
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(RpcError::Node {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| RpcError::Protocol("missing result".into()))
    }

    // ---- typed conveniences (only what the daemon needs) ----

    pub fn get_blockchain_info(&self) -> Result<Value> {
        self.call(None, "getblockchaininfo", json!([]))
    }

    pub fn get_best_block_hash(&self) -> Result<String> {
        self.call(None, "getbestblockhash", json!([]))?
            .as_str()
            .map(String::from)
            .ok_or_else(|| RpcError::Protocol("getbestblockhash: not a string".into()))
    }

    pub fn get_zmq_notifications(&self) -> Result<Value> {
        self.call(None, "getzmqnotifications", json!([]))
    }

    pub fn list_wallets(&self) -> Result<Vec<String>> {
        let v = self.call(None, "listwallets", json!([]))?;
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn load_wallet(&self, name: &str) -> Result<Value> {
        self.call(None, "loadwallet", json!([name]))
    }

    /// Create a blank, keyless descriptor wallet (the automated-import path).
    pub fn create_watchonly_wallet(&self, name: &str) -> Result<Value> {
        // createwallet(name, disable_private_keys, blank, passphrase,
        //              avoid_reuse, descriptors, load_on_startup)
        self.call(
            None,
            "createwallet",
            json!([name, true, true, "", false, true, true]),
        )
    }

    pub fn get_wallet_info(&self, wallet: &str) -> Result<Value> {
        self.call(Some(wallet), "getwalletinfo", json!([]))
    }

    /// Forwarded straight to Core; the descriptor is NEVER persisted or
    /// logged by the daemon (invariants 5, 6). Short timeout on purpose:
    /// `importdescriptors` rescans synchronously and can run for hours on
    /// mainnet — a timeout here means "rescan started", and the caller
    /// watches `getwalletinfo.scanning` rather than blocking the daemon.
    pub fn import_descriptors(&self, wallet: &str, requests: Value) -> Result<Value> {
        self.call_with_timeout(
            Some(wallet),
            "importdescriptors",
            json!([requests]),
            Some(Duration::from_secs(10)),
        )
    }

    /// Node-level mempool probe (no wallet context). Ok(true) = the tx is in
    /// the node's mempool right now; a -5 Node error = it is not.
    pub fn in_mempool(&self, txid: &str) -> Result<bool> {
        match self.call(None, "getmempoolentry", json!([txid])) {
            Ok(_) => Ok(true),
            Err(RpcError::Node { code: -5, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Used transiently for gap-limit bumps (the daemon re-reads the
    /// descriptor from Core rather than storing it).
    pub fn list_descriptors(&self, wallet: &str) -> Result<Value> {
        self.call(Some(wallet), "listdescriptors", json!([]))
    }

    pub fn list_since_block(&self, wallet: &str, blockhash: Option<&str>) -> Result<Value> {
        let params = match blockhash {
            Some(h) => json!([h, 1, true]),
            None => json!([Value::Null, 1, true]),
        };
        self.call(Some(wallet), "listsinceblock", params)
    }

    pub fn get_transaction(&self, wallet: &str, txid: &str, verbose: bool) -> Result<Value> {
        self.call(Some(wallet), "gettransaction", json!([txid, true, verbose]))
    }

    pub fn get_balances(&self, wallet: &str) -> Result<Value> {
        self.call(Some(wallet), "getbalances", json!([]))
    }
}

#[cfg(test)]
mod tests {
    use super::b64;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"__cookie__:abc123"), "X19jb29raWVfXzphYmMxMjM=");
    }
}
