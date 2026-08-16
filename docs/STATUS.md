# STATUS — honest milestone map

Built against `docs/btc-watch-daemon-design.md`. This file is the no-spin
accounting of what exists, what deviates from the spec, and what doesn't
exist. If it isn't listed as done here, assume it isn't done.

Milestone numbers follow `docs/startup_docs/btc-watch-daemon-milestones.md`
(the build-order doc). A 2026-06-12 independent review (`docs/gaps.md`)
found and fixed defects in the never-executed seams; the notes column
reflects the post-fix state.

## Milestones

| Milestone | State | Notes |
|---|---|---|
| M0 skeleton: schema, node connection, init | **done** | schema per §8.2; init refuses IBD (except regtest) and nodes without ZMQ pubs; `SATSURV_ZMQ_ENDPOINTS` override / 0.0.0.0→RPC-host rewrite for cross-host deploys |
| M1 detection core: ZMQ trigger, RPC decides, event rows | **done** | cookie auth; per-topic sequence-gap detection; RPC-authoritative passes; per-outpoint dedup (one tx spending many watched outpoints events each one) |
| M2 outbox + first notifier (ntfy) | **done** | fan-out, two-stage ack; ntfy publishes via JSON body (UTF-8 alarm titles deliver — verified on a live regtest run) |
| M3 HTTP API + CLI + acknowledge + access auth | **done** | role-scoped secrets, 403 on wrong scope; ack = control scope + single-use HMAC capability token; 403 on token reuse |
| M4 reconciliation + onboarding & arming | **done** | arming gate w/ stale-observation guard; automated import fixed (checksum-safe change-branch expansion, no label, ranged-only); blank watch wallets are never adopted or armed; long mainnet rescans run async (`importdescriptors` short-timeout + scanning poller) |
| M5 pre-authorization & false-positive suppression | **done** | pending→consumed/expired/cancelled; preauthed spend auto-resolves, no alarm; expiry re-arms + notifies; both endings checkpoint |
| M6 robustness: tiers, heartbeat, degraded, SMTP, notifier health | **done** | node watchdog with grace (armed never lies → blinded); consolidated ~hourly re-notice; delinquency + tier-coverage sweeps; heartbeat schedule persisted across restarts |
| M7 remote ack path | **partial** | the ntfy action button carries the capability token, **but the acknowledge endpoint also requires the control-scope secret (invariant 8), which the phone does not have — the button as shipped gets 401**. Needs a design decision (see gaps.md "M7 button auth"); CLI ack works. TLS ingress (`tailscale serve`) is yours to provide either way |
| M8 web UI | **not built** | CLI + API only |

All 40 unit tests pass (`cargo test`; needs `libsqlite3-dev` + `libzmq3-dev`).
The regtest demo (`scripts/regtest-demo.sh`) is the integration check; it was
executed end-to-end on 2026-06-12 (after harness fixes) and passes, including
the alarm physically reaching ntfy and the pre-auth suppressing one.
**Signet soak has not been done** — the design's regtest → signet → mainnet
progression stops at regtest today.

## Documented deviations from the spec

1. **Escalation (§13).** The spec's staged 5-minute escalation ladder
   (push, then email, then repeat) is simplified to **eager fan-out**: every
   alarm goes to every notifier covering the alarm tier immediately, then
   repeats every `alarm_repeat_secs` until acknowledged. Simpler, and
   strictly noisier than spec — it errs toward waking you up.
2. **Backstop reconciliation.** Implemented as arithmetic on the delta: if
   an armed wallet's trusted balance drops below its reconciled anchor by
   **more than the summed value of the spends the detector evented** since
   the anchor, that's an immediate `backstop_discrepancy` alarm. The spec's
   fuller per-UTXO set comparison is still not implemented.
3. **SMTP notifier is plain SMTP to a relay.** No TLS, no fancy auth — by
   design. Point it at localhost msmtp/postfix and let the relay own
   transport security. Do not point it across a network you don't trust.
4. **Outbox display endpoint.** `GET /outbox` (control scope, read-only)
   returns recent messages with parsed payloads. It is the sanctioned place
   an operator can recover a capability token if the push notification is
   lost.
5. **Wallet deletion (EDGE 4).** A wallet with any history (events,
   pre-auths, notifications) cannot be deleted — `409 wallet_has_history`;
   disarm it instead. The audit trail is kept indefinitely per §13.

## Known gaps / sharp edges

See `docs/gaps.md` for the full review record (what was found, what was
fixed, what remains). Standing items:

- **M7 phone-button ack cannot authenticate** (see milestone table) —
  open design decision.
- `confirm`'s stale-observation guard compares the observed-at instant you
  echo back; automation that blindly replays the latest value defeats the
  point. The CLI makes you type it on purpose.
- Descriptor secrecy is enforced in this codebase (never persisted/echoed),
  but the descriptor necessarily lives in your **node's** watch wallet —
  that's where the watching happens. Your node's datadir is in the trust
  boundary.
- One SQLite writer behind a mutex, held across node RPC calls in the
  worker. Fine for this workload (the hours-long rescan case is handled
  async); don't point a load tester at it.
- Notifier registration is open to anyone holding the notifier secret, and
  the `?label=` identity means one leaked secret can impersonate any
  notifier. If a secret leaks, `satsurvd rotate-secrets` (daemon stopped)
  regenerates both role secrets and the capability-token key; restart
  notifiers with the new `notifier.secret`.
