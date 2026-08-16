# STATUS — honest milestone map

Built against `docs/btc-watch-daemon-design.md`. This file is the no-spin accounting of what exists, what deviates from the spec, and what doesn't exist. If it isn't listed as done here, assume it isn't done.

Milestone numbers follow `docs/startup_docs/btc-watch-daemon-milestones.md` (the build-order doc). `docs/gaps.md` is the settled record of the June 2026 review and everything that came out of it.

Last updated 2026-08-15.

## Milestones

| Milestone | State | Notes |
|---|---|---|
| M0 skeleton: schema, node connection, init | **done** | schema per §8.2; init refuses IBD (except regtest) and nodes without ZMQ pubs; `SATLANCE_ZMQ_ENDPOINTS` override / 0.0.0.0→RPC-host rewrite for cross-host deploys. Both refusals observed working against a real syncing signet node |
| M1 detection core: ZMQ trigger, RPC decides, event rows | **done** | cookie auth; per-topic sequence-gap detection; RPC-authoritative passes; per-outpoint dedup (one tx spending many watched outpoints events each one) |
| M2 outbox + first notifier (ntfy) | **done** | fan-out, two-stage ack; ntfy publishes via JSON body (UTF-8 alarm titles deliver — verified on a live regtest run, and again against a real ntfy instance) |
| M3 HTTP API + CLI + acknowledge + access auth | **done** | role-scoped secrets, 403 on wrong scope; ack = control scope + single-use HMAC capability token; 403 on token reuse |
| M4 reconciliation + onboarding & arming | **done, one open defect** | arming gate w/ stale-observation guard; blank watch wallets are never adopted or armed; long mainnet rescans run async. **But multipath descriptors import half-watched — see Known gaps.** |
| M5 pre-authorization & false-positive suppression | **done** | pending→consumed/expired/cancelled; preauthed spend auto-resolves, no alarm; expiry re-arms + notifies; both endings checkpoint |
| M6 robustness: tiers, heartbeat, degraded, SMTP, notifier health | **done, one unproven** | node watchdog with grace (armed never lies → blinded); consolidated re-notice; delinquency + tier-coverage sweeps; heartbeat schedule persisted across restarts. **The escalation done-when — an alarm reaching email when ntfy goes unacked — has never been demonstrated; see Known gaps** |
| M7 remote ack path | **decided, not built** | The ntfy action button POSTs the acknowledge with only the capability token and no control-scope credential, so it has always returned 401. **Decided 2026-08-15:** the button becomes an ntfy `view` action deep-linking to the web UI, which already holds the control-scope secret. Invariant 8 is preserved intact; no new credential. Lands with M8. CLI ack works today. TLS ingress (`tailscale serve`) is yours to provide either way |
| M8 web UI | **not built** | CLI + API only |

## Test status

All 40 unit tests pass (`cargo test`; needs `libsqlite3-dev` + `libzmq3-dev`).

`scripts/regtest-demo.sh` is the acceptance check and passes end to end — most recently on 2026-08-15 after the `satsurv` → `satlance` rename, including the alarm physically reaching ntfy, capability-token acknowledgement, and a pre-authorized spend resolving with no alarm.

The signet stage of the design's regtest → signet → mainnet progression now exists: `deploy/docker-compose.signet.yml` plus the runbook at `docs/signet-soak.md`. The node is synced, the daemon is running, and both alert channels have been preflighted against real infrastructure (`scripts/signet-preflight.sh`). **The soak itself is in progress and has not yet produced a result.** Mainnet remains untouched.

## Documented deviations from the spec

1. **Escalation (§13).** The spec's staged 5-minute escalation ladder (push, then email, then repeat) is simplified to **eager fan-out**: every alarm goes to every notifier covering the alarm tier immediately, then repeats every `alarm_repeat_secs` until acknowledged. Simpler, and strictly noisier than spec — it errs toward waking you up.
2. **Backstop reconciliation.** Implemented as arithmetic on the delta: if an armed wallet's trusted balance drops below its reconciled anchor by **more than the summed value of the spends the detector evented** since the anchor, that's an immediate `backstop_discrepancy` alarm. The spec's fuller per-UTXO set comparison is still not implemented.
3. **SMTP notifier is plain SMTP to a relay.** No TLS, no auth to the outside world — by design, so the unit stays tiny and auditable and credentials never ride out of it. Point it at a relay you control and let the relay own transport security. The signet harness demonstrates the intended shape: a postfix container holds the upstream credentials and does mandatory STARTTLS, while the notifier speaks cleartext to it over a private network only.
4. **Outbox display endpoint.** `GET /outbox` (control scope, read-only) returns recent messages with parsed payloads. It is the sanctioned place an operator can recover a capability token if the push notification is lost, and the mechanism the web UI will use to acknowledge.
5. **Wallet deletion (EDGE 4).** A wallet with any history (events, pre-auths, notifications) cannot be deleted — `409 wallet_has_history`; disarm it instead. The audit trail is kept indefinitely per §13.

## Known gaps / sharp edges

`docs/gaps.md` holds the full record of the June 2026 review. Standing items, worst first:

- **Multipath descriptors import half-watched (open, ship-blocking).** A descriptor in `<0;1>` multipath notation passes the ranged check but never triggers the change-branch expansion, so it imports one descriptor covering the receive branch only. `importdescriptors` reports success and the initial balance is correct, because first funding lands on a receive address. The gap opens after the first spend, when change moves to an unwatched branch and silently leaves the watched set. **Multipath is what Sparrow exports by default**, so it is the most likely thing a real user pastes. Measured against a live signet node: `ismine` is true for a receive address and false for the first change address. Workaround: rewrite `/<0;1>/*` to `/0/*` before registering. This is an R3-class failure and worse in one respect — R3 showed a zero balance a human would question, this shows a correct one.
- **The SMTP notifier has never executed.** The binary exists and is built, but nothing has ever run it: the regtest harness starts only the ntfy notifier, and `cargo test` reports zero tests for that crate. The signet stack runs it for the first time. Until an alarm or degraded notice actually reaches a mailbox *through the notifier*, the escalation path is unproven — the relay being verified proves only the transport downstream of it.
- **No runtime config surface.** The `config` table receives its defaults at schema creation and nothing writes it afterwards. Changing `vanished_timeout_secs`, `heartbeat_secs`, or any other tunable requires a manual `sqlite3` UPDATE against a database the daemon holds open behind a mutex, which contradicts M3's "no manual DB pokes" and is a poor answer for anyone self-hosting.
- `confirm`'s stale-observation guard compares the observed-at instant you echo back; automation that blindly replays the latest value defeats the point. The CLI makes you type it on purpose.
- Descriptor secrecy is enforced in this codebase (never persisted, never echoed), but the descriptor necessarily lives in your **node's** watch wallet — that's where the watching happens. Your node's datadir is in the trust boundary.
- One SQLite writer behind a mutex, held across node RPC calls in the worker. Fine for this workload (the hours-long rescan case is handled async); don't point a load tester at it.
- Notifier registration is open to anyone holding the notifier secret, and the `?label=` identity means one leaked secret can impersonate any notifier. If a secret leaks, `satlanced rotate-secrets` (daemon stopped) regenerates both role secrets and the capability-token key; restart notifiers with the new `notifier.secret`.
- **Verification standard.** Everything above marked done has passing tests and, where noted, a live harness run behind it. That is validation, not proof of fitness for real funds. Nothing here has guarded a mainnet UTXO.
