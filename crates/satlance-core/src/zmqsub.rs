//! ZMQ trigger path (design §6, invariant 4).
//!
//! ZMQ is a TRIGGER, not truth: it can drop messages silently. This thread
//! subscribes to whatever endpoints the node advertised at `init`
//! (getzmqnotifications → meta), treats every message as an opaque "go look"
//! poke to the worker, and tracks the per-topic 4-byte LE sequence counter to
//! detect gaps. A gap is just another trigger (the RPC pass is authoritative
//! and self-healing); it's logged and counted for /status honesty.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::Duration;

use log::{info, warn};

use crate::worker::Trigger;

/// Spawn the SUB thread. `endpoints` come from meta (stored at init);
/// duplicates are deduped (Core often advertises one endpoint per topic).
pub fn spawn(endpoints: Vec<String>, tx: Sender<Trigger>) {
    std::thread::Builder::new()
        .name("zmq-sub".into())
        .spawn(move || run(endpoints, tx))
        .expect("spawn zmq thread");
}

fn run(mut endpoints: Vec<String>, tx: Sender<Trigger>) {
    endpoints.sort();
    endpoints.dedup();
    if endpoints.is_empty() {
        warn!("zmq: no endpoints configured — running RPC-poll only (slow mode)");
        return;
    }

    let ctx = zmq::Context::new();
    let sub = match ctx.socket(zmq::SUB) {
        Ok(s) => s,
        Err(e) => {
            warn!("zmq: socket create failed ({e}) — RPC-poll only");
            return;
        }
    };
    // Subscribe to everything the node publishes; we only care THAT something
    // happened, not what (the RPC pass decides). Topic frame is still read
    // for sequence-gap accounting.
    let _ = sub.set_subscribe(b"");
    let _ = sub.set_rcvtimeo(5_000); // ms; periodic wake to notice shutdown/teardown

    for ep in &endpoints {
        match sub.connect(ep) {
            Ok(_) => info!("zmq: connected {ep}"),
            Err(e) => warn!("zmq: connect {ep} failed: {e}"),
        }
    }

    let mut last_seq: HashMap<String, u32> = HashMap::new();
    loop {
        match sub.recv_multipart(0) {
            Ok(frames) => {
                // Frame layout: [topic, body, sequence(LE u32)]
                let topic = frames
                    .first()
                    .map(|f| String::from_utf8_lossy(f).into_owned())
                    .unwrap_or_default();
                if let Some(seq_frame) = frames.get(2) {
                    if seq_frame.len() == 4 {
                        let seq = u32::from_le_bytes([
                            seq_frame[0], seq_frame[1], seq_frame[2], seq_frame[3],
                        ]);
                        if let Some(prev) = last_seq.get(&topic) {
                            let expected = prev.wrapping_add(1);
                            if seq != expected {
                                warn!(
                                    "zmq: sequence gap on '{topic}' (expected {expected}, got {seq}) — RPC pass will reconcile"
                                );
                                let _ = tx.send(Trigger::ZmqGap);
                            }
                        }
                        last_seq.insert(topic, seq);
                    }
                }
                // The poke. Coalescing happens worker-side (it drains).
                if tx.send(Trigger::Zmq).is_err() {
                    return; // worker gone — daemon shutting down
                }
            }
            Err(zmq::Error::EAGAIN) => {
                // recv timeout: quiet period. Liveness accounting is the
                // worker's job (it compares trigger timestamps).
                if tx.send(Trigger::ZmqIdle).is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!("zmq: recv error: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}
