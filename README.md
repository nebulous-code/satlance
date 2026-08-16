# Sat Surveillance

A self-hosted tripwire for Bitcoin cold storage. `satsurvd` watches **your
own** Bitcoin Core/Knots node and screams — push notification, email,
whatever you wire up — the moment any UTXO you've told it to guard moves
on-chain. Detection and alerting only: **no keys, no signing, no spend
capability anywhere in this codebase.**

The threat model is simple: if your cold storage spends and *you* didn't do
it, every minute of warning matters. If you did do it, you pre-authorize the
outpoint first and the spend resolves silently.

## What it is (and isn't)

- Watches spends by **outpoint identity**, never by amount heuristics.
- ZMQ is the trigger, RPC is the truth — a missed ZMQ message degrades
  latency, never correctness.
- Your wallet **descriptor passes through** to your node's watch-only wallet
  and is never persisted, logged, or echoed by this software.
- `armed` never lies. If the node is unreachable, armed wallets are marked
  **blinded** and you're told, loudly. Anything not armed shows as
  **NOT PROTECTED** — there is no quiet middle state.
- Arming always requires you to confirm the observed balance. It never
  auto-arms, even after a restart.

## Architecture

Three kinds of deployable units, talking only over published HTTP contracts
(`docs/CONTRACTS.md`):

```
bitcoind/knots ──RPC+ZMQ──▶ satsurvd (core daemon)
                              │  SQLite: wallets, events, preauths, outbox
                              │
              control scope ──┤◀── satsurv (CLI), your scripts
             notifier scope ──┴──▶ satsurv-notifier-ntfy   (push)
                                   satsurv-notifier-smtp   (email via relay)
```

- **Core** (`crates/satsurv-core`, binary `satsurvd`): detection, event
  lifecycle, escalation, degraded-condition tracking, the outbox, and the
  HTTP API. Channel-blind — it has no idea what ntfy or SMTP are.
- **Notifiers** (`crates/notifier-*`): stateless. Long-poll the outbox,
  deliver, ack in two stages (`received` → `delivered`). Kill one and
  nothing is lost; the core flags it delinquent if acks go overdue.
- **CLI** (`crates/satsurv-cli`, binary `satsurv`): a control surface over
  the same API anyone else could write.

Two bearer secrets are generated at `init` (`control.secret`,
`notifier.secret`, written 0600 beside the DB; only hashes are stored).
A notifier's secret *physically cannot* call control verbs — 403 at the
door. Alarms additionally mint a single-use HMAC **capability token** that
rides only in the alarm payload; silencing an event requires control scope
*plus* that token.

## Building

Rust 1.75+, plus two system libraries (rusqlite links the system SQLite;
the `zmq` crate links libzmq):

```
sudo apt-get install libsqlite3-dev libzmq3-dev pkg-config   # Debian/Ubuntu
cargo build --release
```

## Quick start (against your real node)

Your node needs ZMQ enabled:

```
zmqpubrawtx=tcp://127.0.0.1:28332
zmqpubhashblock=tcp://127.0.0.1:28333
```

Then:

```
SATSURV_RPC_URL=http://127.0.0.1:8332 \
SATSURV_RPC_COOKIE=~/.bitcoin/.cookie \
satsurvd init
```

```
satsurvd run
```

In another shell (the CLI reads `./control.secret` by default):

```
satsurv wallet add cold --descriptor 'wpkh([fingerprint/84h/0h/0h]xpub.../0/*)'
```

```
satsurv status
```

When the import scan finishes, the wallet sits in `awaiting` with an
observed balance. Verify that number against what you believe you hold,
then:

```
satsurv balance confirm 1 --observed-at <timestamp shown in status>
```

Now it's **armed**. Planning a legitimate spend? Pre-authorize the exact
outpoint first:

```
satsurv preauth add 1 <txid> <vout> --note "moving to new vault"
```

The API binds `127.0.0.1:8330` and refuses `0.0.0.0`. For phone access
(ntfy ack buttons), front it with `tailscale serve` — that's the supported
remote path.

## Try it without touching your node

A fully disposable regtest stack (bitcoind + core + ntfy + notifier):

```
./scripts/regtest-demo.sh
```

This walks the whole story end to end: register → import → confirm → arm →
unauthorized spend → alarm → ntfy delivery → token acknowledge → preauthed
spend resolving silently. Requires docker compose, curl, jq.

## Repo map

- `crates/satsurv-core` — daemon (`satsurvd`)
- `crates/satsurv-cli` — control CLI (`satsurv`)
- `crates/notifier-ntfy`, `crates/notifier-smtp` — delivery units
- `deploy/` — Dockerfiles + regtest compose
- `docs/` — design spec, `CONTRACTS.md`, `STATUS.md`
- `scripts/regtest-demo.sh` — acceptance walkthrough

## Status

Implemented through the MVP milestone (M6) with documented deviations and
known gaps — read `docs/STATUS.md` before trusting it with anything that
matters, and run the regtest demo first.

MIT licensed.
