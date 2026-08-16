# CLAUDE.md — working on this repo

Read `docs/btc-watch-daemon-design.md` (the spec) and `docs/CONTRACTS.md`
before changing anything. `docs/STATUS.md` is the honest state of the world;
keep it honest when you change things.

## The one structural rule

**Separate deployable units, coupled only by the published HTTP contracts.**

- `satlance-core` must never know what a delivery channel is. No ntfy, no
  SMTP, no webhooks in the core. It writes to the outbox; that's it.
- Notifiers must be stateless and killable: long-poll `/outbox/pending`,
  deliver, two-stage ack. All durable state lives in the core's SQLite.
- Control surfaces (CLI, future web UI) speak only the control API.
- If you're tempted to import one crate from another to "share a struct,"
  stop — the contract is JSON over HTTP, on purpose.

## Invariants you must not break (spec §10, abridged)

1. Detection is by **outpoint identity**, never amount.
2. ZMQ triggers, RPC decides. Missing ZMQ messages may delay, never corrupt.
3. No descriptor/xpub/address is ever persisted, logged, or echoed by this
   software. They pass through request bodies to the node and die there.
4. `armed` never lies: node loss blinds armed wallets and says so.
5. Never auto-arm. A human confirms the observed balance, every time.
6. Not-armed is loud (NOT PROTECTED), never a quiet default.
7. Notifier-scope credentials physically cannot call control verbs.
8. Acknowledging an alarm needs control scope **plus** the event's
   single-use capability token.
9. Integer sats internally; ISO-8601 only at the wire boundary.
10. Multi-row state changes happen in single SQLite transactions
    (`BEGIN IMMEDIATE`).

## Practical notes

- Toolchain floor is rustc 1.75. `Cargo.lock` pins `url 2.5.0`,
  `indexmap 2.7.1`, `rayon 1.10.0`, `rayon-core 1.12.1` for that reason —
  if you regenerate the lockfile, re-apply those pins or raise the
  toolchain.
- `cargo test` must stay green; new behavior gets a test in the same module.
- Error codes in API responses are stable contract. Add codes; never rename.
- The regtest harness (`scripts/regtest-demo.sh` +
  `deploy/docker-compose.regtest.yml`) is the acceptance check. If your
  change touches detection, arming, or the outbox, run it.
