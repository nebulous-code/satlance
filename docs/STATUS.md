# STATUS — honest milestone map

Built against `docs/btc-watch-daemon-design.md`. This file is the no-spin
accounting of what exists, what deviates from the spec, and what doesn't
exist. If it isn't listed as done here, assume it isn't done.

## Milestones

| Milestone | State | Notes |
|---|---|---|
| M0 skeleton: DB schema, config, init/run | **done** | schema per §8.2; init refuses IBD (except regtest) and refuses nodes without ZMQ pubs |
| M1 node access: RPC + ZMQ subscriber | **done** | cookie auth; per-topic sequence-gap detection triggers full pass |
| M2 wallet lifecycle: register/import/arm | **done** | descriptor pass-through, never persisted; change branch expanded; gap-limit bump-and-rescan on dispute |
| M3 detection: outpoint spends, confirmations | **done** | RPC-authoritative passes; ZMQ/idle/nudge coalesced triggers; 30s tick floor |
| M4 events: alarm, escalate, capability ack | **done** | single-use HMAC token; repeat escalation until acked; 403 on token reuse |
| M5 outbox + notifiers: fan-out, two-stage ack | **done** | delinquency + tier-coverage sweeps; ntfy + smtp units shipped |
| M6 degraded honesty: blinding, NOT PROTECTED, heartbeat | **done** | node watchdog with grace; consolidated ~hourly re-notice; armed never lies |
| M7 remote ack path | **partial** | ntfy action button carries the token and POSTs to `PUBLIC_API_URL`; you must provide TLS ingress (`tailscale serve`) yourself |
| M8 web UI | **not built** | CLI + API only |

All 36 unit tests pass (`cargo test`). The regtest demo
(`scripts/regtest-demo.sh`) is the integration check; it has **not** been
executed in this build environment (no docker available) — run it before
relying on the stack.

## Documented deviations from the spec

1. **Escalation (§13).** The spec's staged 5-minute escalation ladder
   (push, then email, then repeat) is simplified to **eager fan-out**: every
   alarm goes to every notifier covering the alarm tier immediately, then
   repeats every `alarm_repeat_secs` until acknowledged. Simpler, and
   strictly noisier than spec — it errs toward waking you up.
2. **Backstop reconciliation.** Implemented as a delta check: if an armed
   wallet's trusted balance drops below its reconciled anchor with zero
   events recorded since the anchor, that's an immediate
   `backstop_discrepancy` alarm. The spec's fuller per-UTXO set comparison
   is not implemented.
3. **SMTP notifier is plain SMTP to a relay.** No TLS, no fancy auth — by
   design. Point it at localhost msmtp/postfix and let the relay own
   transport security. Do not point it across a network you don't trust.
4. **Outbox display endpoint.** `GET /outbox` (control scope, read-only)
   returns recent messages with parsed payloads. The spec implies control
   may read the outbox; the concrete route shape here is an implementation
   choice. It is the sanctioned place an operator can recover a capability
   token if the push notification is lost.

## Known gaps / sharp edges

- `confirm`'s stale-observation guard compares the observed-at instant you
  echo back; automation that blindly replays the latest value defeats the
  point. The CLI makes you type it on purpose.
- Descriptor secrecy is enforced in this codebase (never persisted/echoed),
  but the descriptor necessarily lives in your **node's** watch wallet —
  that's where the watching happens. Your node's datadir is in the trust
  boundary.
- One SQLite writer behind a mutex. Fine for this workload; don't point a
  load tester at it.
- Notifier registration is open to anyone holding the notifier secret.
  If a secret leaks, `satsurvd rotate-secrets` (daemon stopped) regenerates
  both role secrets and the capability-token key; restart notifiers with
  the new `notifier.secret`.
