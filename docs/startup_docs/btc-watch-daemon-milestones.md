# Sat 'Lance — Build Milestones (proposed order)

Companion to the design doc. This is **what gets built when** (order, not dates), not how. Each milestone lists its goal, what it builds, what it deliberately does **not** yet cover (the honesty line), what it depends on, and whether it's usable yet.

## Sequencing philosophy

- **Vertical slice first, then robustness.** Get one thin path working end-to-end (node → detect → tell me) before broadening. This de-risks the single most uncertain thing — can the daemon reliably detect a spend against your own node — before investing in everything around it. If that doesn't work, nothing else matters; prove it first.
- **Never ship false confidence.** The design's cardinal rule (a wallet that looks protected but isn't) applies to milestones too: every stage is explicit about what it does *not* yet cover, so an early build is never mistaken for a finished guard. "Honest about its gaps" beats "looks done."
- **Each milestone is independently demonstrable.** You can stop after any one and have something that does a real, nameable thing — not a half-wired layer.
- **Contracts are stable from first use.** The outbox and control contracts (design §4.0) get exercised starting at M2/M3; treat them as versioned interfaces from the moment they exist, even informally, so later clients don't force a rewrite.

## Cross-cutting: test environment (set up alongside M0–M1)

Detecting spends safely requires a way to *trigger* spends without risking real funds. Build and test on **regtest first** (instant blocks, mine and spend on demand — ideal for exercising the detection loop deterministically), then **signet** (realistic, shared, still valueless), and only point at **mainnet watch-only with real cold storage last**. Knots supports all three. This isn't a milestone; it's the harness M1 onward is developed against.

---

## M0 — Skeleton, node connection, schema

**Goal:** the daemon boots, reaches your node, and persists state. Foundation for everything.

**Builds:** project skeleton (Rust binary); `init` CLI action (verify RPC reachable + node past IBD + ZMQ publishing, write node config to SQLite); open/create the SQLite DB with the full schema (design §8); structured logging that never echoes xpub/addresses.

**Not yet:** no detection, no notifications, no API. It connects and remembers, nothing more.

**Depends on:** nothing.

**Usable?** No — but it proves you can talk to Knots and persist, which every later stage assumes.

**Done when:** `init` against a regtest node succeeds (RPC reachable, past IBD, ZMQ confirmed publishing), the SQLite DB is created with the full schema, and a restart cleanly reopens it. `init` against an unreachable/syncing node refuses with a clear message naming the failed check.

---

## M1 — Detection core (proof of concept)

**Goal:** prove the entire premise — your node tells you when a watched UTXO moves.

**Builds:** import a watch-only wallet into Core *by hand*; a minimal watched-wallet row in SQLite; ZMQ subscription as the trigger; on trigger, query Core (`listsinceblock`/`gettransaction`) to decide whether a watched UTXO was spent; write an `event` row. Output is a **log line** — no notification path yet. Exercised on regtest by spending a watched UTXO on demand.

**Not yet:** no notifications, no API, no reconciliation, no pre-auth, no arming lifecycle (the wallet is crudely inserted, not onboarded), no false-positive suppression. It will "alarm" (log) on *any* spend including your own.

**Depends on:** M0.

**Usable?** Not for real use, but this is **the heart** — if M1 works, the concept is validated and the rest is plumbing around a proven core. The highest-risk milestone; everything after is lower-uncertainty.

**Done when:** on regtest, with a watch-only wallet imported and one watched UTXO, spending that UTXO produces an `event` row and a log line within seconds of the spend hitting the mempool (via ZMQ trigger → RPC confirm). A spend of an *unwatched* UTXO produces nothing. Killing the daemon mid-window and restarting still catches the spend via RPC reconciliation (no silent miss).

---

## M2 — Outbox + first notifier (first real alert)

**Goal:** a detected spend reaches your phone. First genuinely useful build.

**Builds:** `outbox` + `outbox_delivery` + `notifier`/`notifier_tier` tables; the fan-out delivery model; notifier registration + long-poll pull + two-stage ack (received → delivered); the **ntfy notifier** (your loud path) as a separate deployable unit. Detected spend → outbox → ntfy → phone buzzes.

**Not yet:** only one notifier (no escalation, no SMTP); no tiers beyond what's needed to route an alarm; no heartbeat; no reconciliation; still alarms on your own spends (no pre-auth); ack is not yet possible (no API). One-way: it shouts, you can't yet answer.

**Depends on:** M1.

**Usable?** Yes — minimally. A crude but real "my node detected a spend and my phone buzzed." Annoying (false alarms on your own moves) but it does the core job.

**Done when:** a watched-UTXO spend on regtest results in an ntfy push to a real device, via the full path (event → outbox → `outbox_delivery` row → notifier pull → received-ack → delivered-ack). Stopping the ntfy notifier, triggering a spend, then restarting the notifier still delivers the alert (it waited in the outbox, not lost). Two notifiers subscribed to the same tier both deliver independently.

---

## M3 — HTTP API + CLI + acknowledge + access auth

**Goal:** make it operable — stop poking the DB by hand; be able to answer an alarm.

**Builds:** the local HTTP/JSON API (design §11.1) with payload conventions (§11.2); the **CLI control surface**; wallet add/list, event list, and the **acknowledge** verb (silence a firing alarm); **role-scoped access auth** from the API's birth — control-scope and notifier-scope secrets (design §10.2), enforced at the credential. Replaces M1/M2's manual DB pokes.

**Not yet:** acknowledge is **CLI-only / localhost** (no phone-button ack — that needs capability tokens + TLS, M7); no TLS (localhost only); no web UI; still no reconciliation/pre-auth/heartbeat.

**Depends on:** M2.

**Usable?** Yes, and now *operable* — you manage it through commands and can silence an alarm from the box (or over your tailnet).

**Done when:** wallets and events are managed entirely through the CLI (no manual DB pokes); a firing alarm can be acknowledged via CLI and stops escalating; a request with a notifier-scope secret to a verb endpoint is rejected `403`, and a control-scope secret to a notifier endpoint is likewise rejected; an unauthenticated request is rejected.

---

## M4 — Reconciliation + onboarding & arming

**Goal:** safe, honest setup and ongoing balance tracking. The arming gate makes "protected" mean something.

**Builds:** the observed/confirmed baseline model (design §8.3); `balance/confirm` + `balance/dispute` verbs; reconciliation checkpoints; the **arming state machine** (registered → importing → awaiting → armed, plus blinded/disarmed/error, design §5); daemon-automated descriptor import; the **arming gate** (balance-confirm to arm); **loud not-protected visibility** in status. The first-run balance confirmation *is* the first reconciliation.

**Not yet:** no pre-auth (so a confirmed/armed wallet still alarms on your own spends); no heartbeat digest yet; no degraded-state handling.

**Depends on:** M3 (verbs, wallet lifecycle via API), M1 (Core balance queries).

**Usable?** Yes, and now *safe to set up* — wallets go through an honest lifecycle and can't silently appear protected when they aren't. Largest milestone; could split into M4a (reconciliation baseline + confirm/dispute) and M4b (arming/onboarding using it) if it runs long. *(Input welcome — see questions.)*

**Done when:** adding a wallet walks registered → importing → awaiting → armed, and is `armed` *only* after explicit balance confirmation; a wallet mid-import shows NOT PROTECTED; killing the node connection flips an armed wallet to `blinded` (NOT PROTECTED, distinct from disarmed) and it auto-recovers to `armed` with a catch-up reconciliation when the node returns; "no, keep scanning" bumps the gap limit and re-presents; status output never shows a not-watching wallet as `armed`.

---

## M5 — Pre-authorization & false-positive suppression

**Goal:** stop crying wolf on your own spends. The thing that makes the tool *livable* long-term.

**Builds:** pre-auth verbs and the `pending → consumed/expired/cancelled` lifecycle (design §8); benign-spend detection (a spend matching a pending pre-auth → `auto_resolved`, baseline updated, no alarm); pre-auth expiry + re-arm notice. Now you can move cold-storage funds without tripping the alarm.

**Not yet:** still single-notifier (no escalation/SMTP) and no heartbeat/degraded until M6.

**Depends on:** M4 (baseline for benign updates), M3 (pre-auth verbs), M1 (detection to match against).

**Usable?** Yes — and now it won't train you to ignore it. This is the alert-fatigue killer; arguably where it becomes a tool you'd actually keep running.

**Done when:** pre-authorizing a specific UTXO then spending it produces *no* alarm (event auto-resolves as benign, baseline updates, change re-enters the watched set); spending a *different*, non-pre-authorized UTXO still alarms; a pre-auth left unspent past its TTL expires, re-arms the UTXO, and emits the "lapsed" notice; the pre-auth correctly retires on consumption (single-use).

---

## M6 — Robustness: tiers, escalation, heartbeat, degraded, 2nd notifier, notifier health

**Goal:** make it *trustworthy and resilient*, not just functional. The backstops.

**Builds:** the three notification tiers + per-tier cadence (design §9.1); the **SMTP notifier** (second deployable unit, so escalation is real); cross-notifier escalation ladder (ntfy → email → repeat); the **heartbeat** with balance digest ("silence is itself an alarm"); **degraded-state handling** (failure-mode policy §7.1 — node unreachable → blinded, ZMQ→RPC fallback, etc.); recheck/re-notify/recovery notices; **notifier health / delinquency** detection (§9.3, degraded tier).

**Not yet:** no remote phone-button ack (M7); no web UI (M8).

**Depends on:** M2 (outbox/notifier), M4 (balance for heartbeat), M5 (events to tier).

**Usable?** Yes — this is the **MVP completion line**. Detects, alerts loudly, doesn't false-alarm, tracks balance honestly, survives node/channel failures, and tells you when it can't see. A tool you can trust to run unattended (acking via CLI/tailnet).

**Done when:** an alarm escalates ntfy → email when the first is unacked; the heartbeat arrives on schedule with the balance digest and flips to DEGRADED when a degraded condition is active; killing the node produces a degraded notice (not a theft alarm) and the heartbeat reflects it; a notifier that registered for a tier but stops pulling is flagged delinquent *by name* on the degraded tier after its window; recovery notices fire when conditions clear; the missing-heartbeat case is observable (silence is detectable).

---

## ▲ MVP line: M0–M6 ▲

A trustworthy single-instance daemon: honest detection, loud alerts, no false alarms, balance tracking, resilient backstops. Runs localhost or over your tailnet; alarms reach your phone via ntfy; you acknowledge via CLI.

---

## M7 — Remote hardening (unlock phone-button ack & safe exposure)

**Goal:** safely answer alarms from your phone, and safely reach the API off-box.

**Builds:** TLS via `tailscale serve`; **per-notification capability tokens** on acknowledge (design §10.2); full role-scoped enforcement reviewed for remote exposure; ntfy action-button ack wired to the capability-token flow. The §10.2 auth stack, completed.

**Not yet:** still no web UI.

**Depends on:** M3 (auth basics), M2 (phone alerts to answer).

**Usable?** Yes — now you can dismiss an alarm by tapping your phone, securely, from anywhere on your tailnet.

**Done when:** an alarm can be acknowledged by tapping an ntfy action button on the phone, over TLS (`tailscale serve`), and the dismissal requires the per-notification capability token (a request without it, or with a token for a different event, is rejected); the token is single-use; the API is reachable over the tailnet but not the public internet.

---

## M8 — Web UI (second control surface)

**Goal:** accessibility — manage and monitor without the CLI.

**Builds:** a self-hosted web GUI wrapping the same API (no core changes); status/wallet/event views; acknowledge/confirm from the browser; Stage-1 **notifier health display** (who's delivering, who's silent — reads `delivered_by`/delivery rows through the core).

**Depends on:** M3 (API), M6 (health data to display).

**Usable?** Yes — the friendly front end; everything it does, the CLI already could.

**Done when:** the web UI performs the core operations (view wallets/events/status, acknowledge, confirm balance) against the same API the CLI uses, with no core changes; notifier health (who delivered, who's silent) is visible; it presents the control-scope secret to authenticate.

---

## Future (post-MVP, per design §4.1)

- **Stage-2 Config contract:** notifiers advertise their settings (+ per-field mutability) at `/config`; the web UI renders config forms ("enter SMTP credentials here"). Unlocks non-technical setup.
- **More notifiers:** Signal, Telegram, smart-home, etc. — each a small client against the published outbox contract, zero core change.
- **More control surfaces:** desktop, iOS — each speaks the published control contract.
- **Stage-3 (pie in the sky):** notifier discovery/"App Store" — distribution + trust + sandboxing. Out of scope; the realistic payoff is *published contracts* so the community builds notifiers/controllers without the original author.

---

## Open sequencing questions (your input)

1. **Where's *your* usable line?** I drew MVP at M6, but you might start using it earlier — crudely at M2 (alarms, no ack), or once livable at M5. Knowing where you'd actually start relying on it could pull a milestone's polish forward.
2. **Auth timing.** I put role-scoped access auth at M3 (the API's birth) rather than deferring — a security tool shouldn't run unauthenticated even locally, and it's cheap. Agree, or defer basic auth and run localhost-trusted until M7?
3. **M4 split?** Reconciliation + arming is the biggest single milestone. Build as one, or split M4a (reconciliation) / M4b (arming/onboarding)?
4. **SMTP timing.** I placed the second notifier at M6 (when escalation becomes real). If you want email as a *backup channel* sooner (in case ntfy setup is flaky early), it could move to M2/M3.
5. **Heartbeat earlier?** A minimal "still alive" heartbeat could be pulled into M2/M4 ahead of the full tiered version in M6, since it's the liveness backstop. Worth it, or fine to wait for M6?
