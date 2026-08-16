# Startup Prompt — Sat 'Lance

Paste this at the start of a build session. It orients you and lists the rules you must not violate. Two companion documents define the full design and build order; read them before writing code.

## What you're building

**Sat 'Lance** is a self-hosted daemon that watches a user's own Bitcoin node (Core or Knots) for any spend of a UTXO they control, and alerts them in real time — so an unauthorized movement of cold-storage funds is caught within seconds. It is **detection and alerting only**: it holds no keys, signs nothing, and never constructs transactions. The node's watch-only wallet is the source of truth for what's watched; the daemon adds detection, alert delivery, and accountability around it.

## How to read the docs

- **`btc-watch-daemon-design.md`** — the *how*. Architecture, schema, state machines, contracts, security model. Authoritative for design decisions.
- **`btc-watch-daemon-milestones.md`** — the *what, and in what order*. Start at **M0** and build milestone by milestone; each has a "Done when" acceptance check. Do not skip ahead or build a later milestone's concerns into an earlier one.
- When design and your instinct disagree, the design doc wins — every decision in it was made deliberately. If something seems wrong, flag it; don't silently diverge.

## Architectural constraint — separate deployable units (commit this to memory)

This system is **three kinds of program that communicate only through published contracts**, not one app:
1. **Core (daemon)** — owns all state (SQLite), node access, detection, the outbox, and the HTTP API. Channel-blind.
2. **Notifiers** — egress clients that deliver outbox messages to a channel (ntfy, SMTP). Stateless; outbox-only; issue no commands.
3. **Control surfaces** — the CLI (and later a web UI) that issue verbs and read state.

**Build these as separate, independently deployable units (separate binaries/containers where applicable) from the start — never a monolith you later have to split.** They talk only over the contracts (HTTP). This boundary is load-bearing and easy to erode if forgotten.

**→ Please record this separate-deployable-units architecture in your long-term/project memory (e.g. a `CLAUDE.md` or equivalent) now, so it persists across sessions and survives context resets.** It is the constraint most likely to be violated by a well-meaning shortcut.

## Invariants — never violate these

**Bitcoin / detection**
1. A spent UTXO (outpoint) is **terminal** — destroyed forever, never reused, never receives change. Change always lands on a *new* outpoint.
2. Detection is by **outpoint identity, never by amount**. Amounts are for human-facing messages only — never for deciding whether a spend is yours or authorized.
3. **Outpoint matching is the theft detector; balance reconciliation is the backstop.** Don't make the backstop do the detector's job.
4. **ZMQ is a trigger, not truth** — it can drop messages. The authoritative answer comes from querying the node via RPC. Track ZMQ sequence gaps and reconcile. Treat "seen in mempool" and "confirmed N-deep" as distinct states.

**Source of truth / privacy**
5. **Core/Knots is the source of truth for the watched set** (owns xpub, derivation, UTXO set, reorgs). The daemon stores **no xpub/descriptor** — only a wallet nickname + a Core wallet reference.
6. **Never log, echo, or persist** xpubs, descriptors, or addresses. Response/error payloads structurally exclude them.

**State honesty**
7. **`armed` never lies** — it means "configured to watch AND currently able to." If the node is unreachable, the wallet is `blinded`, not `armed`. Status must never contradict protection reality.
8. **Arming requires explicit user balance confirmation. Never auto-arm.**
9. **Not-armed is loud.** Any wallet not `armed` is surfaced as NOT PROTECTED. Silence means "all armed," never "one's half-configured and forgotten."
10. **The trust signal is the declaration, not the absence:** `reconciled_at IS NULL` = never reconciled (don't trust the balance); `ack_window_secs IS NULL` = explicitly declared fire-and-forget. Absence-as-signal must be a deliberate declaration, never an accident.

**Security**
11. **Triggering an alarm is auth-free and loud; silencing one requires authentication.** An attacker should be able to make it ring but never make it stop. Fail safe toward "can't dismiss" (annoying), never "never fired" (dangerous).
12. **Role-scoped credentials:** notifier-scope secrets work only on notifier endpoints; control-scope on verb/query endpoints. A notifier must be *physically incapable* of issuing a verb (`403`).

**Boundaries**
13. **The core is channel-blind:** it knows a notifier's *identity* (label) and *tier subscription*, never its *nature* (what ntfy/SMTP is or how it works).
14. **Notifiers hold no state and issue no verbs.** If a notifier seems to need more than the outbox contract, that's a **core gap to fix in the core**, never a reason to widen the notifier.
15. **Delivery is fan-out, not claim** — every notifier subscribed to a tier delivers independently; none blocks another. No exclusive leases.
16. **The core never papers over a user's coverage gap** — it doesn't invent notifiers, reroute around a down channel, or change the heartbeat cadence to compensate. It reports honestly; redundancy is the user's deployment responsibility.

**Data / correctness**
17. **Money is integer satoshis everywhere.** Never a float/decimal BTC in logic or on the wire — format to BTC only at display.
18. **Atomicity:** multi-row state changes (e.g. detecting a spend: open event, retire pre-auth, update baseline) wrap in a single transaction. A security tool must never have torn state after a crash.

## Test strategy

Detection correctness is the thing most worth testing hard — a silent false-negative (a real spend that never alarms) is the worst possible failure. Develop and test against **regtest first** (instant blocks; mine and spend watched UTXOs on demand to exercise the detection loop deterministically), then **signet** (realistic, valueless), and only point at **mainnet watch-only with real cold storage last**. Prioritize automated tests around the detection loop, reorg handling, and the reconciliation math.

## How to proceed

Start at **M0** in the milestones doc. Build to each milestone's "Done when" check before moving on. Keep the three program types as separate deployable units throughout. Ask before diverging from the design doc.
