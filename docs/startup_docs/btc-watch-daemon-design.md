# Cold-Storage Spend Monitor — High-Level Design

A self-hosted daemon that watches your own Bitcoin node for any spend of a UTXO you control and alerts you in real time, so an unauthorized movement of cold-storage funds is caught within seconds. Detection and alerting only — no key custody, no transaction construction, no RBF.

---

## 1. Goals & Non-Goals

**Goals**
- Detect, in real time, when any UTXO belonging to a watched wallet is spent.
- Alert via multiple channels (ntfy push + SMTP email) with escalation.
- Run unattended for long periods; fail loud, never fail silent.
- Hold no private keys. Watch-only. Safe to run even if the host is compromised.
- Run in Docker, built for personal use first. (StartOS/Start9 packaging parked — see §14.)
- Talk to your own node (Core or Knots) — no third-party block-explorer APIs.

**Non-Goals**
- No spending, signing, or RBF. Sparrow/Coldcard remain the source of truth and the only things that touch keys.
- Not a wallet. The node-side wallet is a read-only watch list, nothing more.
- Receiving funds is not a security event (alert optional, never escalated).
- No third-party block-explorer APIs — own node only (privacy/trust model for cold storage).
- No hot-wallet watching, no Lightning, no multi-user/multi-tenant.
- No desktop GUI. CLI first; a self-hosted **web GUI is in scope long-term** (as a wrapper over the daemon API — see §11), but not v1.
- No mobile app (notifications reach mobile via ntfy/push, not a native app).
- StartOS/Start9 packaging parked (§14).

---

## 2. Trust Model

- **Sparrow + Coldcard** = source of truth. Generate addresses, hold keys, build spends.
- **Node watch-only wallet** = downstream, disposable. Holds only the public descriptor (xpub). Reveals history/addresses (privacy-sensitive) but cannot move funds.
- **Daemon core** = listener + detector + state; notifiers and control surfaces are separate clients (§4). No keys, and no xpub/descriptor in its DB (§6) — only wallet nicknames + a Core wallet reference. Privacy-sensitive material lives in Core, not the daemon.
- **Weakest link is the alert path.** If an attacker can suppress the alert, the tool is worthless. Heartbeat + independent channels are mandatory. Dismissal is authenticated while triggering is not (§10.2), against an opportunistic threat model (§10.1).

---

## 3. Core Concepts

- **UTXO = a bill.** No balances exist on-chain, only discrete chunks locked to addresses.
- **Spend = a UTXO of mine appears as a transaction INPUT.** This is the alarm condition.
- **Receive = one of my addresses appears as an OUTPUT.** Informational only.
- **Inputs are not self-describing** — an input is a pointer (txid:index) to an earlier output. Deciding "is this input mine?" requires *state*: the node's watch-only wallet remembers which chunks are ours. That is the entire reason the watch-only wallet exists.
- **Spends are terminal.** A UTXO consumed as an input is destroyed permanently; it can never be reused or receive change. Change always lands on a *new* UTXO at a new outpoint.

---

## 4. Architecture Overview

### 4.0 Architectural philosophy — three program types, three contracts

The system is not one app; it is **three kinds of program communicating only through published contracts.** This is the overarching design philosophy and the thing every other decision must respect.

**Three program types:**
- **Core (daemon)** — the single source of truth. Owns detection, all state (SQLite), node access, and the outbox. **Channel-blind:** it never learns what notification technologies exist. It decides *that* a message must go out and records *whether* it did; it never decides *how* it goes out. One per deployment.
- **Notifiers (egress)** — deliver outbox messages to some end technology, then ack. A notifier is a *pipeline to a delivery tech*, not the tech itself: the ntfy notifier is a small program that reads the outbox and POSTs to an ntfy server (which does the phone push); the SMTP notifier is credentials + a POST to a mail provider — neither is a server you run. A single notifier may fan out to multiple end technologies (e.g. a smart-home notifier that flashes lights *and*, on higher severity, sounds a speaker) — that multi-delivery is the notifier's own *how*, not the core's concern. Notifiers hold no core state and issue no commands.
- **Control surfaces** — issue verbs (acknowledge, pre-authorize, …) and read state (wallets, events, status). CLI (v1), web UI / desktop / mobile later. May *read* the outbox to surface alerts inline, but are fundamentally ingress+query, not delivery.

**Three contracts (the actual product — publish these and the ecosystem builds itself):**
1. **Outbox contract** (core ↔ notifier): long-poll `GET /outbox/pending` + `POST /outbox/{id}/ack`, over the channel-agnostic outbox payload (§9). A new notification tech is a small program against this contract, zero core change.
2. **Control contract** (core ↔ control surface): the verb + query endpoints (§11.1). A new controller (web, iOS, desktop) is any program that speaks these, zero core change.
3. **Config contract** (control surface ↔ notifier): a notifier advertises its settings (e.g. `GET /config` → "I need url, port:number, password") and accepts updates; the controller renders a form and validates input. **Routed around the core** — the core never participates. (Stage 2 below; not MVP.)

**The escalation split** (so the two layers don't conflict): the *core* escalates *across* notifiers — the channel ladder (ntfy → email → repeat), driven by acks and tiers (§9.1). A *notifier* escalates *within* itself — how it renders a tier across its own end technologies (lights then speaker). Core decides which notifiers and in what order; the notifier decides how it personally delivers. Both are "how" at their own level.

**Why this matters:** the core's knowledge never grows. Health dashboards, config UIs, new channels, new controllers, even an eventual ecosystem — all are built *between* clients or *above* them, never by teaching the core about channels. This is what lets the tool outlive its original integrations: you author the *spec and reference daemon*, not every integration.

**Data flow:**


Bitcoin network --> Your node (Core/Knots) --+-- ZMQ (real-time shout) --+--> Daemon core
                                              +-- RPC (ask, cookie auth) -+      |
                                                                                 |
                                  +----------------------------------------------+
                                  | Daemon core (single Rust binary)             |
                                  |  - detection + reconciliation                |
                                  |  - all state in SQLite (config, events,      |
                                  |    pre-auths, wallets, outbox)               |
                                  |  - local HTTP/JSON API                       |
                                  +----------------------------------------------+
                                       ^ verbs + query      | outbox (pull + ack)
                                       | (control scope)     v (notifier scope)
                                  +-----------------+   +-------------------------------+
                                  | Control surface |   | Notifiers (egress, stateless) |
                                  | CLI (v1), web   |   |  ntfy push, SMTP email, …     |
                                  | GUI later       |   |  outbox-only, no verbs        |
                                  +-----------------+   +-------------------------------+
```

- **Node** does the heavy lifting: syncs the chain, recognizes watched addresses.
- **ZMQ (ZeroMQ)** = node's one-way real-time broadcast pipe. The *alarm* path. Fires the instant a tx hits the mempool (unconfirmed). Fire-and-forget; messages can be lost.
- **RPC (Remote Procedure Call, JSON-RPC over HTTP, cookie auth)** = node's question/answer interface. The *reconciliation/backstop* path. Used to catch up after downtime, fill ZMQ gaps, and confirm state.
- **Daemon core** = subscribe to ZMQ, match against watched UTXOs, persist all state in SQLite, reconcile via RPC, and expose a local HTTP API. It decides *that* an alert must go out; it does not itself talk to ntfy/SMTP.

Same machine assumed → cookie auth, no cross-host networking concerns.

### Core / client boundary

The daemon is the core; everything else is a thin client over its HTTP API. The core owns all truth (state, detection, node access, the outbox) and is **channel-blind** — it knows nothing about ntfy, Discord, or any UI. This is what keeps the system extensible: a new notification channel in 2030 is a small adapter, with zero core changes.

Clients come in **two distinct roles** (not one "wrapper" bucket — the distinction is load-bearing):

**1. Notifiers (egress, outbox-only).** Pull pending notifications, deliver to a channel, ack. They are delivery adapters and nothing more. v1: ntfy, SMTP. A notifier may touch *only* `GET /outbox/pending` and `POST /outbox/{id}/ack` — no verbs, no state reads, no DB, no node. Its whole world is "pull → render → deliver → ack."

**2. Control surfaces (bidirectional).** Issue verbs (acknowledge, pre-authorize, confirm-balance, …) and read rich state (wallet list, arming states, event history). v1: CLI; web GUI later. A control surface may also *read* the outbox (so a UI can surface urgent notifications inline) but is fundamentally an ingress+query client, not a delivery adapter.

**The decoupling rule:** notifiers and control surfaces never talk to *each other* and never know each other's shape. They communicate only *through the core* (outbox + endpoints). Drawing a direct line between a UI and a notifier would recreate the coupling the channel-blind core exists to prevent.

**The contract that makes this hold:**
- **The outbox payload is the interface contract** (the load-bearing piece). It is channel-agnostic and carries everything *any* channel needs to render a message — title, body, tier/severity (§9.1), a stable event reference, and the capability token when the notification invites a verb. A new channel must be buildable against this payload alone, with no core change. (Schema: §9.)
- **`what` vs. `how`:** the core decides *what* to say; a notifier decides only *how* to deliver it. The moment a notifier has to decide *what* (read balances, interpret state, choose whether to alert), the boundary is broken — that's always the core's job.
- **Role-scoped credentials** (§10.2): notifier-scope and control-scope are *different secrets*, each valid only on its role's endpoints, enforced at the credential. A notifier that tries a verb gets `403` — the role violation fails at the door, not by the wrapper choosing to behave. (Rationale: ntfy is internet-adjacent; if compromised, its credential must be physically incapable of silencing an alarm.)
- **No state lives in a notifier.** Kill/restart/replace any client freely; the core doesn't notice. A down notifier leaves alerts `pending` in the outbox, not lost (§7 ZMQ-loss problem solved at our own boundary).
- **Conformance:** a notifier is conformant if it touches only its allowed endpoints and holds no state. If a notifier seems to need more, that's a *core gap to fix in the core* — never a reason to widen the notifier.

**Channel management is channel-blind too (Option A).** The core does not model "a channel." It never learns ntfy/SMTP exist. Notifier *configuration* (SMTP creds, ntfy endpoint) belongs to the *notifier* — its own config/admin surface — never the core. The core reports channel *health* only via `outbox.delivered_by` (who's acking, who's gone silent), so a heartbeat/UI can say "delivered by ntfy, email" without the core knowing what those are. See §4.1 for the future configuration/discovery seam (explicitly above the core).

- **Config lives in SQLite, not a separate file** — core config, state, and outbox in one DB. One persistence mechanism.
- **API:** small local HTTP/JSON, localhost by default (Tailscale opt-in). Debuggable with curl; no gRPC/socket-only.

### 4.1 Extensibility roadmap & deployment topology

A staged path from MVP to a community ecosystem. The invariant across all stages: **the core's knowledge never grows** — everything richer is built between clients or above them.

- **Stage 0 (MVP):** controller and notifiers don't talk to each other. Notifiers are hand-configured (config file / env). The controller learns of notifiers only indirectly, as `outbox.delivered_by` evidence in the core's records.
- **Stage 1 (near):** controller reads notifier *health* — still entirely through the core's records (the core tracks "expected-ack-by" and flags silent notifiers, §9.1), no direct controller↔notifier link.
- **Stage 2 (medium):** controller *configures* a notifier via the **Config contract** (§4.0 #3) — a direct controller↔notifier conversation routed *around* the core. Notifier advertises its settings + types (`port` is a number, etc.) **and a per-field mutability** (read-only vs. read-write to control surfaces): a security-critical notifier can expose its tier subscription as read-only so a UI can *see* it watches `alarm` but cannot *disable* that — the notifier owns its own hardening, the UI respects it. (Consequence: a fully read-only notifier is configured only by editing its own config directly — the right tradeoff for a hardened channel.) Controller renders/validates a form and writes back the mutable fields. This is the contract that also lets anyone write a *new* notifier or a *new* controller against published interfaces. Useful for non-technical setup ("enter SMTP credentials here"); the CLI doesn't need it (edit config directly) — the tell that it's a UI convenience, not a core concern.
- **Stage 3 (pie in the sky):** discover/download notifiers — an "App Store." Adds distribution + trust + sandboxing atop Stage 2. **Out of scope**, and architecturally excluded by channel-blindness. But because the three contracts are published, the community can build messengers and controllers without the original author — the realistic payoff is "here's how you make a notifier / a controller," not a literal store.

**Deployment topology — "deployable unit per role-instance":**
- The **separation is enforced by topology:** distinct deployable units can only talk via the contracts (network calls), so the boundary can't be accidentally violated by code reaching across it. This is a feature, not just tidiness.
- **Core:** one container. Owns SQLite, node access, the API.
- **Each notifier:** its own deployable unit (typically a container). A notifier *fronts* an end technology — the ntfy notifier talks to an ntfy server (separate, existing container) and does not *embed* it; the SMTP notifier is an outbound client (no server). One notifier = one unit even if it fans out to several end techs (the smart-home case).
- **Control surfaces vary:** a web UI is its own container; a CLI is an unpackaged binary; an iOS app isn't containerized at all. The Control + Config contracts are identical regardless of packaging — that's the flexibility.
- **Networking:** on one host, a Docker network + service names; across hosts, the tailnet. The contract calls become real network hops (the cost that makes them enforceable).
- **Principle over mechanism:** "container" is the usual mechanism; the actual rule is *independently deployable, contract-only communication*. When something can't be a container (CLI, mobile), the principle still holds — it talks only via contracts.

---

## 5. Setup & Onboarding

Bootstrap's job is the gap between "installed" and "trustworthily watching." Cardinal sin: a wallet that *looks* armed but isn't. The design defeats that with an explicit arming state machine, loud visibility of not-yet-armed states, and conservative gating (never auto-arm, never claim a readiness the daemon doesn't have).

### 5.1 Two levels of readiness (global gates per-wallet)

- **Daemon-global readiness** (one node): RPC reachable **and** node past IBD (not mid-sync) **and** ZMQ publishing. "RPC responds" ≠ "node ready" — a half-synced node answers RPC with incomplete data, so sync height is checked explicitly. Until all three hold, nothing can be armed and the daemon says so rather than pretends.
- **Per-wallet arming** (§5.3): each wallet independently completes import → scan → confirmation.
- Global gates per-wallet: no wallet arms while the node isn't ready.

### 5.2 Daemon bootstrap — node connection (the chicken-and-egg)

Separate from wallet onboarding, and comes first. There's an irreducible minimum the daemon needs *before* it can reach its own DB or the node:

- **From env vars / flags** (the one thing that can't live in SQLite — needed to *reach* SQLite and the node): DB path, node RPC endpoint + cookie path, ZMQ endpoints.
- **CLI `init` verifies before committing:** reach RPC, confirm past IBD, confirm ZMQ publishing — only then write the verified node config into SQLite. On any failure, refuse and name the failing check rather than starting a daemon that silently can't see its node.
- Everything after that minimum (wallets, settings, defaults) is managed through CLI verbs against the running daemon, in SQLite.

### 5.3 Wallet onboarding — the arming state machine

Per-wallet lifecycle, an explicit `wallet.arming_state` column (not implied):

```
registered → descriptor known, not yet imported to the node
importing  → descriptor imported / historical rescan running  (BLIND — see below)
awaiting   → scan done, balance computed, waiting for user yes/no
armed      → confirmed AND currently able to watch; detector live, alarms hot
blinded    → was armed, but node unreachable now; daemon-managed, NOT PROTECTED (involuntary)
(error)    → import/scan failure; explicitly NOT protected
(disarmed) → user deliberately paused it
```

**`armed` never lies — it means "configured to watch *and* currently able to."** If the daemon can't see the node, the wallet is `blinded`, not `armed`. This is deliberate: the alert and the wallet status must never contradict, or the user learns to dismiss a real outage as "a fluke" after glancing at a status that still says armed. State must match protection reality.

**`blinded` vs. `disarmed` — same "NOT PROTECTED", opposite intent, must be visually distinct:**
- `disarmed` = *you* paused it on purpose. Recovery is deliberate (you re-arm, gated by the while-paused delta confirmation, EDGE 2).
- `blinded` = the *daemon* lost the node, no choice of yours. Daemon-managed: auto-enters on node loss, auto-exits to `armed` (with catch-up reconciliation + recovery notice) when the node returns — you never touch it.
- They must look different in heartbeat/status so "I paused that" can't be confused with "something's wrong and I'm dark."

**Why `importing` is not protected:** during the initial rescan the node is still *building* the wallet's UTXO set, so the daemon's picture of "which outpoints are mine" is incomplete — and a detector on an incomplete set produces *false negatives* (misses a real spend), the failure direction that hurts. A wallet is therefore untrustworthy until its first scan completes; this is a real reliability fact, not bureaucracy.

**`registered → importing` supports both paths:**
- **By hand** (learn-the-mechanics, first time): user imports the descriptor into the node; daemon detects the watch-only wallet appearing and picks up at `importing`.
- **Daemon-automated** (reproducible later deploys): hand the daemon the descriptor; it makes the `importdescriptors` call itself.

**Descriptor import details (these live in Core, not the daemon DB — §6):**
- Import the **descriptor (xpub-based)** into Core, not individual addresses — so future change/receive addresses land inside the watched set automatically.
- Import **separate receive and change branches** (`.../0/*` and `.../1/*`). The change branch is where "future UTXO walks past the gap-limit window" bites; under-importing it is the most common silent failure.
- **Range / gap limit** is Core's concern (Core derives and watches the range); set high enough that the watch-only balance reconciles with reality. The daemon verifies by balance match (§5.4), not by holding the descriptor.

### 5.4 Arming gate — explicit balance confirmation

`awaiting → armed` *is* the first-run version of the §8.3 reconciliation flow:

- Daemon proposes "found X BTC — does this look right? [Yes / No, keep scanning / Abort]". Expected balance lives in your head/Sparrow, never in the daemon.
- **Only "yes" arms the wallet.** Arming is never automatic — the same human-in-the-loop confirmation that protects ongoing reconciliation protects initial setup.
- "No, keep scanning" loops back through a gap-limit bump *before* arming, so you can't bake a wrong baseline in on day one.
- The confirmed balance becomes `reconciled_sat` with `reconciled_at` set — the trusted anchor (the null-trust-signal rule, §8.3).

### 5.5 Loud visibility — what actually defeats the trap

The state machine only helps if not-armed states are impossible to miss. Every heartbeat and status output lists each wallet *with its arming state*; any wallet not `armed` is surfaced prominently:

```
WATCHED (armed):
  coldcard-main   0.187 BTC   armed since 5/28
NOT YET ARMED / NOT PROTECTED:
  ⚠ travel-wallet  importing (rescan 60%) — NOT PROTECTED
  ⚠ savings-2      awaiting your balance confirmation — NOT PROTECTED
  ⛔ vault-cold     BLINDED — node unreachable (involuntary) — NOT PROTECTED
  ⏸ spare-wallet   disarmed by you — NOT PROTECTED
```

Principle: an unfinished wallet **nags, never hides**. Silence always means "all armed," never "one's half-set-up and you forgot." This is what makes "looks protected but isn't" structurally impossible — no state implies protection the daemon doesn't have.

### 5.6 Restart is not bootstrap

On restart, `armed` wallets must **not** fall back through onboarding (re-confirming balances every reboot trains rubber-stamping). Instead an armed wallet does the §7 catch-up reconciliation (RPC from last-processed position) and either returns to `armed` (clean) or fires an alert (a spend happened during downtime). Re-confirmation is for genuine first-time arming only, not for resuming.

---

## 6. Detection Logic

**Architecture: Core/Knots is the source of truth for the watched set.** The node's watch-only wallet owns the xpub/descriptor, address derivation + gap limit, the UTXO set, and reorg handling. The daemon does **not** mirror the UTXO set or store the descriptor — it holds only a reference to the Core wallet (§8.2) and asks Core "is this mine?" / "what's the balance?" via RPC. This is a deliberate **data-minimization** choice: the privacy-sensitive material (xpub, derived addresses, traceable history) lives in Core, which is built to guard it, not in the daemon's SQLite. If the daemon's DB leaks, the blast radius is wallet nicknames + a Core wallet reference, not the xpub. Trade-off accepted: the daemon depends on Core's wallet *operationally* (import/rescan/`listsinceblock` correctness), but that is decade-hardened, keyless, and (in Knots) privacy-minded — a safe trust to extend.

**Detection is pull-shaped:** ZMQ (`rawtx`/block) is the *trigger* ("something happened, go look"); the authoritative "is it mine?" answer comes from querying Core. The daemon never decides ownership from its own UTXO mirror, because it keeps none.

Per incoming transaction (ZMQ trigger → confirm against Core; also during RPC reconciliation):

1. ZMQ surfaces a tx/block → query Core (`listsinceblock` / `gettransaction`) to learn whether a watched-wallet UTXO was spent (appeared as an input). Core resolves the outpoint-ownership question.
2. If a watched UTXO was spent → **spend detected**. Check the state store (Section 8):
   - Matches a pre-authorization → benign (expected spend), update baseline.
   - No matching authorization → **alarm**, begin escalation.
3. (Optional) Watched-wallet receive (Core reports a new UTXO to the wallet) → informational.

**Confirmation state machine** (an event progresses through states, not a binary):
- `seen` (0-conf, in mempool) → fire note immediately.
- `confirmed-1` … `confirmed-6` → status updates.
- `final` at 6 confirmations.
- `vanished` — seen in mempool then dropped/replaced, never confirmed. Must resolve the open event (don't leave a fired alarm dangling with no on-chain record).
- `reorged` — confirmed then un-confirmed by a chain reorg; may re-confirm elsewhere.

**Mute switch:** a way to say "I'm aware this is moving" so a known spend doesn't alarm.

---

## 7. Reliability & Backstop

- **ZMQ sequence tracking:** Core appends an up-counting sequence number to notifications. On a detected gap → trigger an RPC reconciliation. This is the line between "reliable" and "missed the one alert that mattered."
- **Persisted position:** last-processed block/position saved in the SQLite `meta` table, so a restart reconciles via RPC from where it left off rather than missing the downtime window.
- **Startup grace:** node may be mid-IBD or RPC not ready — wait/retry, don't crash-loop. This is the daemon-global readiness gate (§5.1); nothing arms until the node is past IBD and ZMQ is live.
- **Rescan awareness:** descriptor import or long reconciliation can trigger a slow historical rescan that blocks other wallet RPC; trigger deliberately, not by surprise.
- **Time sync:** escalation timers, heartbeat intervals, and "seconds since last block" all assume a correct clock. Drifted container clocks make the system lie about latency.

### 7.1 Failure-mode policy

Mechanisms above; this is what the daemon *does* per failure. Governing principle: **inability to confirm safety is itself loud** — degrade toward alarming, never toward quiet. Each failure maps to a notification tier (§9).

- **Node unreachable** (RPC down) → after a short grace (ride out a node restart): **degraded** "not protected — node unreachable." Heartbeat flips to DEGRADED instead of all-clear. Each `armed` wallet flips to `blinded` (§5.3) so per-wallet status matches reality, not just the global notice. Blindness is alarm-worthy in its own right, not a silent log line.
- **ZMQ silent, RPC fine** → fall back to **RPC polling** (higher latency, backstop still covers spends); **degraded** "slow mode." Not full blindness since reconciliation still works.
- **Mid-IBD at runtime** (node fell behind / resyncing) → **degraded**; affected wallets report not-trusted-armed (the §5.1 readiness gate applies at runtime, not just startup).
- **One channel down** (ntfy up, email down or vice-versa) → deliver on the working channel; outbox keeps the failed one `pending` and retries. Not its own alarm; don't mark resolved just because the other channel worked.
- **All channels down** → **alarm** tier, but caught primarily by **heartbeat silence** (§9.1): a missed expected heartbeat is the signal, since a comms failure can't report itself through comms. Alert stays `pending` in the outbox and delivers when any channel returns; a secondary local signal (health-check/exit status) aids discoverability. Cost: up to one heartbeat interval of delay.
- **Disk full / SQLite write failure** → **alarm** tier, immediately. A daemon that can't persist state can't be trusted to remember an alarm — more dangerous than node-blindness, so it ranks above degraded. (Belt-and-suspenders: a box that can't write to disk has larger problems the user will likely notice by other means; this is a backstop, not the primary detector.)

---

## 8. State Store (the spine) — SQLite

Persistence is a single SQLite database (WAL mode), replacing the earlier flat file. It holds the daemon's domain state — watched-wallet nicknames + Core wallet references (not xpubs/descriptors — those stay in Core, §6), pending pre-authorizations, alert events, and last-processed position. One persistence mechanism for all state. `rusqlite` (synchronous, single-writer daemon) is the natural Rust binding.

Two distinct verbs drive it, at two distinct moments:
- **Pre-authorize** (*before* broadcast): "about to move this UTXO, don't alarm."
- **Acknowledge** (*after* an alarm fired): "that was me, stop escalating."

### 8.1 Lifecycles (states first — columns fall out of these)

**Pre-authorization:** keyed on the outpoint detection matches against.
```
pending ──(UTXO consumed on-chain)──> consumed     # natural retirement, expected spend
        ──(timeout, never broadcast)─> expired      # abandoned intent → re-arm + notify
        ──(manual revoke)───────────-> cancelled    # changed my mind
```
Single-use: a `pending` entry can only ever match once, because spends are terminal — the outpoint can never reappear as an input. Timeout governs only the abandoned-intent path, NOT reuse (reuse is impossible).

**Event:** born when a spend of a watched UTXO is detected. Moves along **two independent axes** — do not collapse into one status column:
```
confirmation:  seen ──> confirmed ──> final
                 └────> vanished              # mempool-dropped, never confirmed
               confirmed ──> reorged ──> (seen|confirmed again | vanished)
resolution:    firing ──> acknowledged        # human said "that was me"
                      └─> auto_resolved        # e.g. authorized spend, or vanished+closed
```
An event can be `confirmed`/`firing` simultaneously (on chain, not yet ack'd) or `vanished`/`auto_resolved`, etc. The two axes move independently.

### 8.2 Schema

```sql
-- The watched cold wallets. Parent entity; everything else references it.
CREATE TABLE wallet (
    id              INTEGER PRIMARY KEY,
    label           TEXT    NOT NULL UNIQUE,  -- human nickname, e.g. 'coldcard-main'
    core_wallet_ref TEXT    NOT NULL,         -- name Core/Knots knows this watch-only wallet by (NOT the xpub)
    arming_state    TEXT    NOT NULL DEFAULT 'registered',  -- registered|importing|awaiting|armed|blinded|error|disarmed (§5.3)
    -- Confirmed baseline: the trusted anchor (the balance the USER has blessed).
    reconciled_sat  INTEGER NOT NULL DEFAULT 0,  -- last confirmed-good balance (always a real number)
    reconciled_at   INTEGER,                  -- when reconciled; NULL = never reconciled, do NOT trust reconciled_sat
    -- Observed baseline: what the daemon currently SEES (from Core) but the user hasn't blessed yet. Ephemeral.
    observed_sat    INTEGER,                  -- current computed balance, NULL when nothing pending confirmation
    observed_at     INTEGER,                  -- when observed_sat was computed
    observed_scans  INTEGER NOT NULL DEFAULT 0,  -- rescan count: 1 on first observe, +1 per user dispute
    observed_status TEXT    NOT NULL DEFAULT 'idle',  -- idle|scanning (internal: keeps heartbeat from talking over a rescan)
    added_at        INTEGER NOT NULL
);

-- Pending and historical pre-authorizations.
CREATE TABLE preauth (
    id            INTEGER PRIMARY KEY,
    wallet_id     INTEGER NOT NULL,
    txid          TEXT    NOT NULL,           -- outpoint being authorized to spend
    vout          INTEGER NOT NULL,           -- output index of that outpoint
    state         TEXT    NOT NULL DEFAULT 'pending',  -- pending|consumed|expired|cancelled
    created_at    INTEGER NOT NULL,           -- unix seconds
    expires_at    INTEGER NOT NULL,           -- created_at + configurable TTL
    resolved_at   INTEGER,                    -- when it left 'pending'
    note          TEXT,                       -- optional free text ("moving to new cold wallet")
    FOREIGN KEY(wallet_id) REFERENCES wallet(id),
    UNIQUE(txid, vout, state)                 -- at most one *pending* per outpoint (see note)
);
CREATE INDEX idx_preauth_pending ON preauth(state, expires_at);
CREATE INDEX idx_preauth_outpoint ON preauth(txid, vout);

-- Detected spends of watched UTXOs (the alarm records).
CREATE TABLE event (
    id                INTEGER PRIMARY KEY,
    wallet_id         INTEGER NOT NULL,
    spent_txid        TEXT    NOT NULL,       -- the watched UTXO that was spent (input outpoint)
    spent_vout        INTEGER NOT NULL,       -- its output index
    amount_sat        INTEGER NOT NULL,       -- value of the spent UTXO
    spending_txid     TEXT    NOT NULL,       -- tx that spent it
    confirmation      TEXT    NOT NULL DEFAULT 'seen',    -- seen|confirmed|final|vanished|reorged
    confirmations     INTEGER NOT NULL DEFAULT 0,
    resolution        TEXT    NOT NULL DEFAULT 'firing',  -- firing|acknowledged|auto_resolved
    authorized        INTEGER NOT NULL DEFAULT 0,         -- bool: matched a preauth at detection
    preauth_id        INTEGER,                            -- FK if authorized
    detected_at       INTEGER NOT NULL,
    last_escalated_at INTEGER,                -- drives the escalation timer
    resolved_at       INTEGER,
    FOREIGN KEY(wallet_id)  REFERENCES wallet(id),
    FOREIGN KEY(preauth_id) REFERENCES preauth(id)
);
CREATE INDEX idx_event_open ON event(resolution, last_escalated_at);
CREATE INDEX idx_event_spending ON event(spending_txid);
CREATE UNIQUE INDEX idx_event_dedup ON event(spent_txid, spent_vout, spending_txid);

-- Singleton daemon position / misc state. Replaces the old flat file entirely.
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,                   -- e.g. 'last_block_height', 'last_block_hash',
    value TEXT NOT NULL                       --       'zmq_last_sequence'
);

-- Registered notifiers (egress clients). Identity + what they watch + their ack promise.
-- The core knows a notifier's IDENTITY and TIER subscription, never its channel nature (§4.0).
CREATE TABLE notifier (
    id              INTEGER PRIMARY KEY,
    label           TEXT    NOT NULL UNIQUE,    -- registered identity, e.g. 'ntfy' (opaque string to the core)
    ack_window_secs INTEGER,                    -- promised receive→deliver window; NULL = explicitly declared fire-and-forget
    registered_at   INTEGER NOT NULL,
    last_seen_at    INTEGER                     -- updated on poll/ack; liveness aid
);

-- Which tiers each notifier monitors (it pulls only these). Join table → queryable "who watches alarm?".
CREATE TABLE notifier_tier (
    notifier_id     INTEGER NOT NULL,
    tier            TEXT    NOT NULL,           -- alarm|degraded|heartbeat
    PRIMARY KEY (notifier_id, tier),
    FOREIGN KEY(notifier_id) REFERENCES notifier(id)
);

-- The notification MESSAGE itself (one row), tier-stamped. Delivery is tracked separately (fan-out).
CREATE TABLE outbox (
    id            INTEGER PRIMARY KEY,
    tier          TEXT    NOT NULL,             -- alarm|degraded|heartbeat (the dispatch dimension; notifiers subscribe by tier)
    kind          TEXT    NOT NULL,             -- finer type: spend_alarm|node_unreachable|preauth_lapsed|confirm_prompt|heartbeat|…
    wallet_id     INTEGER,                      -- nullable (heartbeat spans wallets)
    event_id      INTEGER,                      -- FK if this notification concerns an event
    payload       TEXT    NOT NULL,             -- channel-agnostic content (JSON); a notifier renders it
    created_at    INTEGER NOT NULL,
    FOREIGN KEY(wallet_id) REFERENCES wallet(id),
    FOREIGN KEY(event_id)  REFERENCES event(id)
);
CREATE INDEX idx_outbox_tier ON outbox(tier, created_at);

-- Per-(message × notifier) delivery tracking. FAN-OUT, not a claim: every notifier subscribed to
-- the message's tier gets its own row; none blocks the others. Rows are created EAGERLY at enqueue,
-- one per notifier registered for that tier — so "no coverage" = zero rows, and a delinquent
-- notifier = a row stuck in 'pending'.
CREATE TABLE outbox_delivery (
    id              INTEGER PRIMARY KEY,
    outbox_id       INTEGER NOT NULL,
    notifier_id     INTEGER NOT NULL,
    state           TEXT    NOT NULL DEFAULT 'pending',  -- pending → received → delivered
    expected_ack_by INTEGER,                    -- created_at + notifier.ack_window_secs; NULL if fire-and-forget
    received_at     INTEGER,                    -- notifier ack'd "I have it, will deliver" (stage 1)
    delivered_at    INTEGER,                    -- notifier ack'd "delivered" (stage 2)
    FOREIGN KEY(outbox_id)   REFERENCES outbox(id),
    FOREIGN KEY(notifier_id) REFERENCES notifier(id),
    UNIQUE(outbox_id, notifier_id)
);
CREATE INDEX idx_delivery_open ON outbox_delivery(state, expected_ack_by);
```

### 8.3 Reconciliation model — confirmed vs. observed balance

**Design tenet: the code tracks UTXOs; the user tracks wallets.** Internally every decision is outpoint-level (identity-based, can't be spoofed by coincidental amounts). But every time the daemon *talks to you*, it collapses that down to wallet-level balances, because that's the unit you reason in — you don't remember individual outpoints. Thorough on the backend, friendly on the front end. This boundary is why amounts appear at all: they're wrong for *detection* but right for *communication*.

**Outpoint matching is the theft detector; balance reconciliation is the backstop.** The real-time, per-spend check in §6 (does this spent outpoint match a pending pre-auth?) is what actually catches theft — it needs no balance math, and a thief's spend is a different outpoint than your authorized one regardless of amounts. The balance/heartbeat layer is a *consistency check* that catches what the real-time path missed (dropped ZMQ message, daemon downtime, a bug). Don't make the backstop do the detector's job.

**Two baselines.** `reconciled_*` is the trusted anchor — the balance the user has confirmed. `observed_*` is what the daemon currently computes but the user hasn't blessed. They model the gap between "what I last agreed was true" and "what the daemon sees now."

**Observed balance has three conceptual states, but only two rest:**
- `confirmed` — user said yes → `observed_*` promotes into `reconciled_*`, observed columns wipe (`observed_sat`/`_at` → NULL, `observed_scans` → 0). A resting state.
- `awaiting` — daemon has computed a new balance, user hasn't responded. A resting state; carried forward visibly in the heartbeat (non-blocking — see below).
- `disputed` — **not a resting state.** "No, keep scanning" is a transient trigger: it bumps `observed_scans`, kicks an immediate rescan (bump gap limit, recompute), then returns to `awaiting` with whatever the rescan found. The daemon never sits in disputed — disputed just means "do work, come back to awaiting." Telling the daemon you dislike the balance doesn't conjure missing sats; if the rescan still comes up short, that's a *go-open-Sparrow* problem, not a daemon bookkeeping problem.

**Reconciliation is both automatic and prompted, and never blocks.** On a reconciliation checkpoint (below) the daemon computes the balance automatically, then *prompts* "balance is 0.4, confirm? [yes / no, keep scanning]". If the user responds, the baseline advances. If they don't, the daemon neither freezes nor silently trusts itself — it carries the discrepancy forward in every heartbeat as an unconfirmed observation until cleared. An unanswered prompt becomes ambient state, not a blocker.

**Pre-auth resolution is a reconciliation checkpoint — both endings.** A pre-auth ends either `consumed` (you spent it → baseline should drop by that UTXO, change re-enters watched set) or `expired` (you didn't → re-arm, balance unchanged). *Both* trigger a checkpoint: consumed because the balance really changed, expired because "I expected this might move and it didn't" is still worth confirming. Every pre-auth, win or lose, closes with daemon and user agreeing on the new state — that's what keeps the gap from drifting open.

**`observed_status` is internal coordination, not a user feature.** Its only job: keep the heartbeat from reporting a number it's about to revise. If a rescan is slow enough to overlap a heartbeat (and a deep descriptor rescan can be — see §7), `observed_status='scanning'` tells the heartbeat to show "(rescan in progress)" instead of a stale shortfall. Two values (`idle`/`scanning`) suffice for MVP. A user-facing "where's the scan at?" query is explicitly *not* MVP; the column exists for the heartbeat-overlap case, which is why it's kept despite scans normally being immediate/synchronous.

**Heartbeat notation** (renders both baselines + scan context):
```
0.5 reconciled 1/1 (0.4 observed 1/2 — scanned 2×)
0.5 reconciled 1/1 (0.4 observed 1/2 — scanned 2×, no further UTXOs found)
0.5 reconciled 1/1 (rescan in progress)
```
A new UTXO found bumps `observed_sat` (e.g. 0.4 → 0.43) and resets `observed_scans` to 1. Another dispute bumps scans again. "No further UTXOs found" on the awaiting line matters so that "keep scanning" returning the same number doesn't read as "nothing happened" — the daemon looked, and the news is "it's really not here."

### 8.4 How each subsystem uses it (all reads/writes against this store)

- **Wallet registry:** the `wallet` table is the authoritative list of what's watched, by nickname + `core_wallet_ref`. It deliberately does **not** store the descriptor (§6) — Core is the source of truth for the watched set, so "does my copy match Core's?" is not a question the daemon asks; it asks Core directly. Verification is by balance match at the arming gate (§5.4), not by re-deriving from a stored xpub. The `core_wallet_ref` is what "which wallet moved" labeling points at.
- **Detection** (§6): on a spend, look up `preauth` by outpoint where `state='pending'`. Match → insert `event` with `authorized=1` and `resolution` set to `auto_resolved` (or keep `firing` if you want to confirm even authorized spends — config choice), flip the preauth to `consumed`. No match → insert `event` with `authorized=0, resolution='firing'`. The `idx_event_dedup` unique index makes the ZMQ-then-RPC double-hit an idempotent upsert, not two alarms.
- **Confirmation tracking** (§6): RPC reconciliation updates `event.confirmation` / `confirmations`; `vanished` and `reorged` are off-ramp updates to the same row.
- **Acknowledge verb:** `UPDATE event SET resolution='acknowledged' …` — closes escalation on the human axis without touching the confirmation axis.
- **Expiry sweep:** periodic `SELECT … FROM preauth WHERE state='pending' AND expires_at < now` → flip to `expired`, re-arm, notify ("pre-auth lapsed, balance unchanged").
- **Escalation timer:** `SELECT … FROM event WHERE resolution='firing' AND last_escalated_at < now - interval` → send next escalation step, bump `last_escalated_at`.
- **Heartbeat reconciliation** (§9): this is the *backstop*, not the theft detector (§8.3). If `wallet.reconciled_at IS NULL`, no baseline has ever been established — establish one, do not compute a shortfall. Otherwise compare the node's current UTXO set for the wallet against what's expected given `reconciled_sat` and the `consumed` preauths since `reconciled_at`. A discrepancy explained by a consumed preauth → benign; promote to a new baseline (or surface as `observed_*` awaiting confirmation). A discrepancy with *no* explaining record → the real-time outpoint detector missed something → escalate. Trust signal is always `reconciled_at`, never the raw balance.
- **Reconciliation checkpoints:** a pre-auth resolving (`consumed` or `expired`) computes a fresh balance into `observed_*` (`observed_scans=1`, status `idle`) and prompts the user. "Yes" → promote `observed_*` into `reconciled_*`, wipe observed. "No, keep scanning" → set `observed_status='scanning'`, bump `observed_scans`, rescan/recompute, return to `idle` awaiting. No response → carried forward in the heartbeat (non-blocking).
- **Catch-up:** read `meta` last-processed height/hash + `zmq_last_sequence` on startup.

### 8.5 Notes / decisions baked in

- **Atomicity matters here:** detecting a spend touches multiple rows (retire preauth, open event, maybe update the wallet baseline). Wrap in a transaction so a mid-update crash can't leave torn state on a security tool.
- **Baseline folded into `wallet`, not a separate table:** a wallet *has* a reconciled balance — it's an attribute of the wallet, not its own entity. The earlier standalone `baseline` table was doing half the `wallet` table's job awkwardly.
- **`reconciled_sat` is never null; `reconciled_at` is the trust signal.** A balance is always a real number (zero is a legitimate confirmed balance). What can be unknown is whether we've *ever reconciled* — and that's a timestamp that can genuinely be "never" (`reconciled_at IS NULL`). Reasons it may be null: wallet just added, IBD incomplete, crash before the reconciliation pull completed, or the user aborted reconciliation. In all those cases the balance is not yet trustworthy regardless of what number sits in `reconciled_sat`. Don't smuggle "unknown" into the numeric balance field.
- **`UNIQUE(txid,vout,state)` on preauth** enforces "at most one *pending* per outpoint" while still allowing historical rows (a past `consumed` + a new `pending` differ in the state column). Simple guard against double-authorizing the same UTXO.
- **Change handling:** a spent UTXO's change is a *new* outpoint — it re-enters normal watched state automatically and is armed again. It never inherits the preauth; nothing special in the schema, which is the point.
- **`observed_*` columns are ephemeral; promote-and-wipe.** They hold an unconfirmed balance between a reconciliation checkpoint and the user blessing it. On confirmation, `observed_sat`/`observed_at` flow into `reconciled_sat`/`reconciled_at`, `observed_scans` resets to 0, `observed_status` to `idle`, and `observed_sat`/`observed_at` go back to NULL. NULL `observed_sat` = nothing pending. This is the durable confirmed/awaiting split; "disputed" is never stored — it's a transient rescan trigger (§8.3).
- **`observed_status` is internal heartbeat-coordination only.** Not a user-facing feature. Exists so a slow rescan overlapping a heartbeat shows "(rescan in progress)" instead of a stale number. `idle`/`scanning` is enough for MVP.
- **Two baselines, not one:** `reconciled_*` is what the user confirmed (trusted anchor); `observed_*` is what the daemon sees but hasn't been blessed. The gap between them is the standing to-do the heartbeat surfaces.
- **Outbox = durable delivery, not fire-and-forget.** The core writes a message to `outbox` (tier-stamped) and eagerly materializes one `outbox_delivery` row (`pending`) per notifier registered for that tier (§4.0 fan-out). Notifiers pull, receive-ack (`pending→received`), deliver, deliver-ack (`received→delivered`). This solves the ZMQ-loss problem (§7) at our own boundary — a down notifier leaves its delivery row queued, not lost. See §4 (core/client boundary) and §9.
- **No xpub/descriptor in this DB (§6).** The privacy-sensitive material lives in Core, not here — so a DB leak exposes nicknames + a Core wallet reference, not traceable wallet history. The DB still warrants tight permissions and care, but the data-minimization choice keeps the worst-case leak small by design. See §10.
- WAL mode → reads don't block the single writer; on-disk form gains `-wal`/`-shm` sidecars (relevant for the Docker volume mount and backups).

### 8.6 State transition tables

The authoritative *from → trigger → to + side-effect* tables. States are defined in §8.1/§5.3; these pin the triggers and side-effects so nothing is left to guess. `‹EDGE n›` marks an edge-case choice walked through below.

**Wallet arming** (`wallet.arming_state`, §5.3)

| From | Trigger | To | Side-effect |
|------|---------|----|-------------|
| (none) | `POST /wallets` | `registered` | row created; xpub forwarded to Core if automated-import, never stored |
| `registered` | descriptor import begins (by-hand detected, or automated call) | `importing` | Core rescan running; wallet is BLIND/NOT PROTECTED |
| `importing` | Core rescan completes | `awaiting` | compute balance from Core → `observed_*`; prompt user (§5.4) |
| `importing` | import/rescan fails, or node lost mid-scan | `error` | degraded notice; NOT PROTECTED `‹EDGE 1›` |
| `awaiting` | user confirms balance (`balance/confirm`) | `armed` | promote `observed_*`→`reconciled_*`; detector live |
| `awaiting` | user disputes (`balance/dispute`) | `awaiting` | bump gap limit (Core) + rescan; `observed_scans++` (stays `awaiting`) |
| `armed` | `POST /wallets/{id}/disarm` | `disarmed` | watching paused; surfaced as NOT PROTECTED in heartbeat |
| `disarmed` | `POST /wallets/{id}/arm` | `armed` | resume; run a catch-up reconciliation first `‹EDGE 2›` |
| `armed` | node lost / unreachable | `blinded` | daemon-managed; NOT PROTECTED but distinct from `disarmed` (involuntary, not a choice); paired with daemon-global degraded (§7.1) `‹EDGE 3›` |
| `blinded` | node reachable again | `armed` | auto-recover (no user action); run catch-up reconciliation first; recovery notice (§9.1) |
| `error` | `POST /wallets/{id}/import` or `/rescan` retry | `importing` | re-attempt |
| any | `DELETE /wallets/{id}` | (removed) | row deleted; open events/pre-auths handled per `‹EDGE 4›` |

**Event — confirmation axis** (`event.confirmation`, §8.1)

| From | Trigger | To | Side-effect |
|------|---------|----|-------------|
| (none) | spend of watched UTXO detected (Core, via ZMQ trigger) | `seen` | event row created (0-conf); notify per `notify_depths` |
| `seen` | reaches 1 confirmation | `confirmed` | update `confirmations`; notify if 1 ∈ `notify_depths` |
| `confirmed` | reaches `final` depth (6) | `final` | update; notify if 6 ∈ `notify_depths` |
| `seen` | dropped from mempool > vanished-timeout (14d), never confirmed | `vanished` | resolve open event (§9); do not leave a fired alarm dangling |
| `confirmed` | block reorged out | `reorged` | re-evaluate via Core; may return to `seen`/`confirmed` or → `vanished` `‹EDGE 5›` |

**Event — resolution axis** (`event.resolution`, §8.1; independent of confirmation)

| From | Trigger | To | Side-effect |
|------|---------|----|-------------|
| (none) | event created, no matching pre-auth | `firing` | begin escalation (§9.1 alarm tier) |
| (none) | event created, matches pending pre-auth | `auto_resolved` or `firing` | per config flag (§8.4): benign auto-close, or still confirm; flip pre-auth → `consumed` |
| `firing` | `POST /events/{id}/acknowledge` (+ capability token) | `acknowledged` | stop escalation; event closed on human axis |
| `firing` | confirmation axis → `vanished` | `auto_resolved` | nothing on-chain to alarm about; close with note `‹EDGE 6›` |

**Pre-authorization** (`preauth.state`, §8.1)

| From | Trigger | To | Side-effect |
|------|---------|----|-------------|
| (none) | `POST /preauths` | `pending` | armed for one specific outpoint; `expires_at = now + TTL` |
| `pending` | its outpoint spent on-chain | `consumed` | the matching spend's event is benign (auto_resolved or confirm); change re-enters watched set |
| `pending` | `expires_at` passes, never spent | `expired` | re-arm the UTXO; notify "pre-auth lapsed, balance unchanged" |
| `pending` | `DELETE /preauths/{id}` | `cancelled` | user changed mind; UTXO stays armed |

---

## 9. Notifications

### 9.1 Three tiers, each with its own configurable cadence

All cadences are config (schema-consistent — even "immediate" is a configurable value).

| Tier | Triggers | Cadence | Channel escalation |
|------|----------|---------|--------------------|
| **Alarm** | spend detected (§6); all-channels-down; disk/write failure | immediate, repeat until acked | yes (ntfy→email→repeat) |
| **Degraded** | node unreachable; mid-IBD at runtime; ZMQ silent (→RPC fallback) — see §7.1 | fire immediately, **re-notify** ~1h, **recheck** ~1min, recovery notice on clear | no (primary channel only) |
| **Heartbeat** | routine all-clear + balance | 6h | no |

**Recheck ≠ re-notify** (two intervals, not one):
- **Recheck** — how often the daemon retests whether the condition cleared. Frequent (~1min); cheap; this is how recovery is detected fast.
- **Re-notify** — how often it reminds you it's still broken. Slow (~1h); don't spam. ("If I'm out at dinner I won't drive home immediately, but an hourly reminder keeps me from forgetting.")

**Recovery notice:** when a degraded condition clears, send a one-shot "protection restored" — the counterpart to the degraded warning; it closes the loop and stops the re-notify timer.

**Degraded conditions consolidate in *presentation*, not in *state*.** Each condition is its own tracked row with its own recheck and its own clear (node-unreachable clears when the node returns; channel-down clears when the channel returns — independently). What consolidates is only the re-notify timer (one hourly reminder listing whatever's currently active) and the heartbeat line. Recovery notice fires *per condition* ("node reachable again"); the overall degraded state lifts only when the *last* row clears. This avoids co-clearing one condition because a different one resolved.

**Degraded re-notifies on primary channel only — unless the primary is what's broken.** "ntfy is down" can't be reported via ntfy; it jumps to the next working channel (email). Rule: primary, except when the degraded condition *is* the primary channel, then fall to the next working one.

**All-channels-down is caught by heartbeat silence.** A notification failure can't report itself through the notification system — but the heartbeat already makes silence meaningful, so a missed expected heartbeat (e.g. no 6am digest) *is* the signal. Cost: up to one heartbeat interval of delay, the inherent price of a non-spammy cadence. A *secondary* local signal (nonzero health-check/exit status, dead-man indicator something external can watch) aids discoverability but is not the primary mechanism. The alert itself stays `pending` in the outbox throughout, so it delivers once any channel returns.

### 9.2 Mechanics

- **Dispatch by tier, fan-out delivery (§4.0, §8).** The core stamps each message with a tier and writes it once; it does not address individual notifiers. Each notifier subscribes to the tiers it cares about (a smart-home notifier might take `alarm` only; email might take `alarm`+`degraded`) and pulls those. Delivery is **fan-out, not a claim**: every notifier subscribed to the tier delivers independently, in parallel, none blocking the others. If ntfy and email both take `alarm`, both deliver the same alarm; ntfy failing never prevents email. (No exclusive leases — a claim model would let one notifier's failure strand a message.)
- **Two-stage ack per (message × notifier):** `received` ("I have it, will deliver") then `delivered`. This lets the core distinguish *never picked up* from *picked up then failed* (§9.3). Both acks are per delivery row, so notifiers never stomp each other.
- **Notifiers are egress clients, not part of the core (§4.0).** v1 notifiers: ntfy (self-hosted, free, open-source push — the "loud" lock-screen path) + SMTP email. True SMS requires a paid/third-party gateway, so it's out; ntfy delivers the *behavior* (loud, immediate, push) without the SMS network. New channels (Signal/Telegram/web GUI) are new clients against the same contract, never core changes.
- **Delivery guarantee:** a notifier deliver-acks only after successful delivery; until then its delivery row stays open. No notification is assumed sent.
- **Confirmation-update notifications are a configurable depth list, not on/off.** A per-config array `notify_depths` names which confirmation depths emit a status update — default `[0, 1, 6]` ("you kicked this off" at 0, "first confirmation" at 1, "finalized" at 6). `[6]` = final only; `[]` = none. A list/dictionary, not a boolean.
- **Escalation:** the *core* escalates *across* notifiers (the ranked channel ladder, ack-driven); a *notifier* escalates *within* itself (rendering a tier across its own end-techs). A single failed notifier must not eat the alert.
- **De-duplication:** one spend hits twice (ZMQ 0-conf, then RPC confirmation). Track fired events → "one event, escalating status," not two panics.
- **Heartbeat** carries the balance breakdown, e.g. "Service running, sees 0.524 BTC across [wallet table]." Doubles as backstop proof that the real-time path didn't silently die. **Silence is itself an alarm.** Flips to DEGRADED (not all-clear) when a degraded condition is active.
  - *Critical distinction:* heartbeat cadence ≠ alarm latency. ZMQ alerts in seconds; the heartbeat is the slower "is the alarm still alive + here's the balance" digest. Tune intervals independently.
- **Balance line shows both baselines (§8.3):** the confirmed anchor plus any unconfirmed observation and its scan context. The detector is outpoint matching (§6); this line is the human-facing backstop, not the primary theft check.
  ```
  0.5 reconciled 1/1 (0.4 observed 1/2 — scanned 2×)
  0.5 reconciled 1/1 (0.4 observed 1/2 — scanned 2×, no further UTXOs found)
  0.5 reconciled 1/1 (rescan in progress)
  ```
  - A clean wallet (no pending observation) just shows the reconciled balance.
  - An unconfirmed discrepancy with a matching consumed preauth reads as benign and prompts confirmation; one with **no** explaining record is the theft signal — do not default to the innocent "scan for more" reading.
  - If `observed_status='scanning'` when the heartbeat fires, show "(rescan in progress)" rather than a number about to be revised.
  - Wallet arming states (§5.5) appear here too — any not-`armed` wallet is flagged NOT PROTECTED.

### 9.3 Notifier health — delivery accountability

Because notifiers register an **identity** + their **tier subscription** + an **ack-window** (§8 `notifier`), and delivery rows are created eagerly per subscribed notifier (§8 `outbox_delivery`), the core can hold notifiers accountable *by name* without knowing what they are (channel-blind = blind to nature, not identity). Four states per (message × notifier):

- **No coverage** — a message's tier has *zero* registered notifiers → no delivery rows → nobody can deliver it. A config gap (e.g. you have no `alarm` channel at all). Detectable at enqueue.
- **Delinquent pickup** — a notifier registered for the tier has a delivery row still `pending` past its window → it isn't even reading the outbox (dead/stuck). "Process NTFY (monitors alarm) hasn't picked up an alarm in N min."
- **Stalled delivery** — row is `received` but not `delivered` past the window → picked up, then failed mid-send.
- **Delivered** — terminal.

**How a notifier-health problem is surfaced (degraded tier + heartbeat):** a delinquent/stalled notifier is a **degraded condition** (§9.1) — "a notifier you rely on isn't responding," same family as node-unreachable, not a theft alarm. It re-notifies on the degraded cadence *and* folds into heartbeat content. `ack_window_secs = NULL` (explicitly declared fire-and-forget) opts a notifier out of delinquency/stall detection — the *declaration* is the signal (cf. §8.5 `reconciled_at` null-rule); a notifier that promised a window and missed it is flagged, one that promised nothing is not.

**Don't tell a dead notifier it's dead.** Dispatch is by tier, not by addressing a notifier, so the core never routes a "you're down" notice *to* the down notifier specifically. A notifier-down notice rides the degraded/heartbeat tiers — a *different* set of subscribers. If the user's only `alarm`-tier notifier is the one that's down, the notice surfaces via the heartbeat (whatever monitors `heartbeat`), and if nothing covers that either, that single-point-of-failure is the user's config responsibility: **the core will not invent an extra notifier, reroute around a coverage gap, or change the heartbeat cadence to compensate.** It reports honestly and leaves redundancy to the user's deployment.

**Liveness aid:** a notifier long-polling `/outbox/pending` holds a connection; `notifier.last_seen_at` (updated on poll/ack) is a cheap "is it even connected" signal, but the authoritative accountability is the per-delivery ack tracking above.

---

## 10. Security & Operational Notes

- **Credentials:** RPC cookie (same-host); ntfy token; SMTP password. Injected via env vars (Docker); kept out of logs and images.
- **Logging without leaking:** enough to debug, but never echo xpub or addresses.
- **State store:** SQLite DB (WAL mode) with tight file permissions; small footprint. The `-wal`/`-shm` sidecars live alongside the main DB file — mount the whole directory, not just the one file, in Docker; back up consistently (don't copy mid-write).
- **Connection lifecycle:** ZMQ self-heals on node restart; RPC calls need retry logic.

### 10.1 Threat model

The realistic adversary is **opportunistic**: someone obtained your keys (leaked seed, compromised signer, found a backup) and is moving coins. Critically, that attacker has *spending* capability but **no foothold in the alert path** — not on your tailnet, not on your phone, not reading daemon traffic. The spend channel (Bitcoin network) and the alert channel (your notifications) are **disjoint** for this adversary.

This sets the security bar: defend against passive interception and casual spoofing of the dismiss path; do **not** contort daily UX for the targeted attacker who holds *both* your keys *and* your phone/tailnet — that case is largely unstoppable at this layer, and defending it would tax every legitimate dismissal forever for a threat you'll likely never face.

### 10.2 Verb authentication

**Asymmetry is the core principle: triggering an alarm is auth-free and loud; *silencing* one requires auth.** The daemon screams on any detected spend regardless of authentication. Only dismiss/acknowledge needs to be authenticated. This also fails safe — a bug in the auth layer means "can't dismiss" (annoying), never "alarm silently never fired" (dangerous). Split by stakes: **pre-authorize** (before a spend, low stakes) can ride a convenient channel; **acknowledge** (silences a live alarm, high stakes) gets the stronger checks below.

**v1 stack (four cheap layers, no GPG-level ceremony):**
- **Role-scoped credentials** — the daemon issues *separate secrets per client role* (§4): a **notifier-scope** secret valid only on `GET /outbox/pending` + `POST /outbox/{id}/ack`, and a **control-scope** secret valid on the verb + query + outbox-read endpoints. Same secret *type*, different secret and different allowed-endpoint set, enforced at the credential. A notifier that attempts a verb gets `403` — the role violation fails at the door, not by the wrapper choosing to behave. Rationale: ntfy is internet-adjacent; if compromised, its credential must be *physically incapable* of silencing an alarm.
- **TLS everywhere** — free via `tailscale serve`. Defeats the "sniff a plaintext request midflight and replay it" scenario outright; there's no plaintext and the sniffer has no token.
- **Per-notification capability token** — each alert embeds a one-time, single-use token scoped to *that event only* (an HMAC over the event ID with a daemon-side secret; sign on the way out, verify on the way back). A leaked or observed token dismisses one already-fired alarm and nothing else — kills replay and blast radius, with no key management. (Layered *on top of* the control-scope secret for the silencing verb.)
- **Tailscale device identity** — since the tailnet already authenticates devices cryptographically (WireGuard), the daemon accepts verb calls only from specific enrolled tailnet nodes. Most of the "it's really my phone" guarantee for almost no code. Authenticates the *device*, not the *person* (an unlocked stolen phone still reads as yours — the targeted case, deliberately out of scope).

**Deliberately out of scope for v1:** out-of-band second factor (PIN/CLI step on dismissal). It's the only thing that helps against "attacker has my unlocked phone," but it taxes every legitimate dismissal — wrong trade for the opportunistic threat model.

### 10.3 Single-tenancy — no account system

**Single-tenant by deployment: one instance per person/household, on their own box behind their own tailnet.** This is deliberately *not* a shared service (unlike Jellyfin/Immich). The boundary between users is the *deployment boundary* (a separate daemon instance), not a login inside a shared one.

Consequence: the daemon needs **access auth** ("is this request allowed?") but not **identity/account auth** ("who are you?"). The §10.2 stack is access auth and is therefore complete — there is no second user to distinguish from, so accounts would only add a login system to protect you from yourself. It's also the better privacy posture: a friend who wants this runs their own copy against their own node rather than enrolling their wallet in your instance. Absence of accounts is the privacy model, not a gap.

The daemon issues **role-scoped secrets** (§10.2: notifier-scope, control-scope), generated at first run, that the matching client presents to use its allowed endpoints. This is still *access* auth, not accounts — "is this request allowed on this role's endpoints?", never "who are you?". The eventual web GUI "login" is just presenting the control-scope secret (optionally with a session cookie) — it *feels* like a login but has zero account machinery behind it: a yes/no on the right secret, no user records.

---

## 11. Implementation Notes

- **Language:** Rust. Daemon-shaped, strong types for raw tx bytes, single deployable binary.
  - `bitcoincore-rpc` (typed JSON-RPC), `zmq`/`tmq` (ZMQ), `bitcoin` + `miniscript` (parse txs, descriptors, address/outpoint matching), `rusqlite` (state store), `lettre` (SMTP, in a wrapper), `reqwest` (ntfy POST, in a wrapper). Local HTTP API via `axum`/`hyper`. Tokio is the natural backbone given the HTTP server + ZMQ + timers.
  - libzmq linking is the one build wrinkle (matters if you ever cross-compile to ARM).
- **Core / wrapper split (§4):** the core binary owns detection, reconciliation, all SQLite state, and the HTTP API. Notifier wrappers (ntfy, SMTP) and the CLI are separate thin clients of that API — they hold no state and can be separate processes/containers. `lettre`/`reqwest` live in wrappers, not the core.
- **Config in SQLite, not a file:** config, state, and outbox share one DB. No separate config file to keep in sync.
- **Platform-agnostic core:** the core knows nothing about its host — runs as a plain Docker container. Keeping it host-agnostic leaves the door open to a thin StartOS wrapper later (§14) without coupling the core to it now.
- **API & remote access:** local HTTP/JSON, localhost-bound by default. For phone/laptop access, prefer `tailscale serve` (TLS-terminated reverse proxy to the localhost port, daemon stays on localhost) over binding to the Tailscale interface directly. Tailscale is a WireGuard mesh — tailnet devices reach each other encrypted with no port forwarding and nothing public. Avoid binding `0.0.0.0` on a LAN-connected host (exposes to the whole LAN); avoid `tailscale funnel` (public internet).
- **Node compatibility:** Core/Knots identical for RPC + ZMQ; pin logic to the Core base version a Knots release corresponds to. Use a **descriptor** wallet (not legacy).

### 11.1 API endpoints

Local HTTP/JSON. Hybrid style: REST for resources (wallets, events, outbox), action sub-paths for genuine commands. Slow operations return `202 Accepted` and proceed async; the client watches state via the status/wallet feeds.

**Auth (§10.2, §4):** **role-scoped secrets** — a **control-scope** secret for the verb + query + outbox-read endpoints (CLI/UI), a **notifier-scope** secret valid *only* on the notifier endpoints (register, pull, ack). Different secret per role, enforced at the credential, so a notifier physically cannot call a verb (`403`). The high-stakes *silencing* verb (`events/{id}/acknowledge`) *additionally* requires the per-notification **capability token** that rode out with that event's alert. Low-stakes verbs (pre-authorize, balance confirm/dispute) need only the control-scope secret — matching the "receiving/triggering easy, silencing hard" asymmetry. Scope is tagged per group below.

**Not an HTTP endpoint:** `init` is a CLI/startup action — it verifies node connectivity (RPC reachable, past IBD, ZMQ live) and writes node config to SQLite *before* the API is serving. It bootstraps the thing that serves HTTP, so it can't be served by it.

```
STATUS / HEALTH (read — control scope; /health may be unauthenticated for monitors)
  GET  /status                 daemon readiness, node/IBD/ZMQ state, active degraded conditions, version
  GET  /health                 lightweight liveness/readiness for external monitors (the all-channels-down backstop watcher)

WALLETS (resource + lifecycle actions — control scope)
  POST   /wallets              register {label, core_wallet_ref, descriptor?} → 202; enters arming state machine (§5.3). descriptor (if given, automated-import path) is forwarded to Core and NOT persisted in the daemon DB
  GET    /wallets              list with arming_state + balances (reconciled + observed)
  GET    /wallets/{id}         detail (incl. arming_state for async polling)
  DELETE /wallets/{id}         remove
  POST   /wallets/{id}/import  daemon-automated descriptor import (§5.3); by-hand path is auto-detected, no call needed → 202
  POST   /wallets/{id}/arm     (re)arm / resume a disarmed wallet
  POST   /wallets/{id}/disarm  pause watching
  POST   /wallets/{id}/rescan  manual gap-limit bump + rescan → 202 (also the target of "keep scanning")

BALANCE RECONCILIATION (§8.3 verbs — control scope)
  POST /wallets/{id}/balance/confirm   {observed_at} "yes" → promote observed→reconciled; arms if in `awaiting`
  POST /wallets/{id}/balance/dispute   "no, keep scanning" → bump+rescan, back to awaiting
       (confirm carries the observed_at it saw, so a stale balance that changed since the prompt isn't blessed)

PRE-AUTHORIZATIONS (§8 verbs — control scope)
  POST   /preauths             create {wallet_id, txid, vout, note?, ttl?}
  GET    /preauths             list (pending + historical)
  DELETE /preauths/{id}        cancel ("changed my mind" → cancelled)

EVENTS (resource + silencing action — control scope)
  GET  /events                 list, filter by confirmation/resolution state
  GET  /events/{id}            detail
  POST /events/{id}/acknowledge  silence/resolve a firing alarm — control scope + REQUIRES capability token

OUTBOX & NOTIFIERS (NOTIFIER scope; control scope may also read for display)
  POST /notifiers/register     {label, tiers:[...], ack_window_secs|null} → register identity + subscription + ack promise
  GET  /outbox/pending         long-poll (held open until a pending delivery for THIS notifier's tiers, or timeout;
                               short-poll fallback). Returns this notifier's own pending delivery rows — fan-out,
                               non-exclusive: other notifiers get their own rows for the same message.
  POST /outbox/{id}/received   stage-1 ack: "I have it, will deliver" (pending→received), starts the ack clock
  POST /outbox/{id}/delivered  stage-2 ack: "delivered" (received→delivered). delivery row keyed by (message, this notifier)
```

### 11.2 Payload conventions

House style, applied to every endpoint (the field *content* of each payload is already determined by the §8 schema + state machines; these are the cross-cutting conventions):

- **Field naming:** `snake_case` everywhere (matches SQLite columns + Rust/serde). A JS web GUI maps at its own boundary.
- **Money:** integer **satoshis** only (`"amount_sat": 18700000`). Never a float/decimal BTC in the API — formatted to BTC only at display time in the wrapper/UI. Avoids floating-point money bugs by construction.
- **Timestamps:** **ISO-8601** strings on the wire (`"2026-06-02T14:30:00Z"`). DB columns stay integer Unix seconds (per §8.2); the daemon converts at the API boundary. "Unix in the DB, ISO on the wire."
- **Errors:** consistent envelope `{"error": {"code": "...", "message": "..."}}` with a **stable machine-readable `code`** string (e.g. `wallet_not_found`, `capability_token_invalid`, `wallet_not_armed`), HTTP status carrying the category (404/409/etc.). Stable codes let wrappers/GUI react programmatically (e.g. re-prompt on `capability_token_invalid`).
- **Async (202) operations:** return the **resource with its current state** (e.g. wallet with `arming_state: "importing"`); the client polls `GET /wallets/{id}`. No separate job/operation abstraction — `arming_state`/`observed_status` are the progress signals.
- **Single vs. list:** `GET /wallets/{id}` returns one object; `GET /wallets` returns a list of the **same-shape** objects. Detail-by-id never returns a filtered list (counterintuitive + needless exposure). Full objects in both (no summary projection — premature at single-user scale).
- **Security invariant — responses structurally exclude sensitive material:** the wallet payload **never** contains the xpub/descriptor, even on the automated-import path where `POST /wallets` *receives* a descriptor (forwarded to Core, not persisted, not echoed back). Error `message` fields and logs never include addresses or descriptor content. Define payload structs to *omit* these fields so they cannot leak by accident (§6 data-minimization, §10).


---

## 12. Prior Art

- **sat.watch** (Go, MIT) — closest existing FOSS tool; node-native via Docker, but sits on an Electrum server (electrs/Fulcrum) for indexing rather than the node's watch-only wallet + ZMQ. Worth studying for the Electrum-index-vs-watch-only-wallet tradeoff.
- Most other "address monitors" are hosted/freemium SaaS or polling scripts against public explorer APIs — wrong trust model for cold storage (leaks addresses to a third party).
- No FOSS tool found matching this exact niche: Rust + own-node + ZMQ spend-detection + watch-only-wallet + self-hosted notify.

---

## 13. Defaults & Tunables

All configurable; these are the v1 defaults.

- **Heartbeat interval:** 6h. (This is blind-window-to-daemon-death, not alarm latency — ZMQ catches live theft in seconds if the daemon is alive.)
- **Pre-auth TTL:** 24h before an unbroadcast pre-auth expires as abandoned (→ re-arm + notify).
- **Escalation:** ntfy immediately → if no ack in 5 min, also email → repeat ntfy every 15 min until acked.
- **Degraded re-notify:** ~1h (reminder cadence while a degraded condition persists).
- **Degraded recheck:** ~1min (how often the daemon retests whether the condition cleared; drives fast recovery detection).
- **Recovery notice:** on (one-shot "protection restored" when a degraded condition clears).
- **Escalation channel order:** ntfy (loud push) → email (backup) → repeat ntfy.
- **Confirmation-notify depths:** configurable list `notify_depths`, default `[0, 1, 6]` (0 = "you kicked this off", 1 = "first confirmation", 6 = "finalized"). `[6]` = final only; `[]` = none. A list, not a boolean.
- **"Final" confirmation depth:** 6.
- **New-wallet starting gap limit / range:** 1000 (generous for cold storage; balance-match verifies sufficiency).
- **Rescan bump on "keep scanning":** double it (1000 → 2000 → 4000 …), cap at 10k, then tell the user "go check Sparrow" rather than scanning further.
- **Mempool "vanished" timeout:** 14 days (matches Bitcoin Core's default mempool expiry, 336h).
- **Retention:** keep resolved events/pre-auths indefinitely (audit trail; data is tiny). No prune functionality in MVP.
- **API bind:** localhost only by default. Remote access via Tailscale opt-in — either bind to the Tailscale interface (`100.x.y.z`) or front it with `tailscale serve` (TLS-terminated, daemon stays on localhost). Never bind `0.0.0.0` on a LAN-connected box; never use `tailscale funnel` (public). See §11.
- **Outbox delivery:** notifiers long-poll; the daemon resurfaces a still-`pending` delivery row every ~30s until that notifier acks. Fan-out — each subscribed notifier has its own row.
- **Notifier pickup grace:** a delivery row `pending` past the notifier's `ack_window_secs` (or a default ~5min if it promised a window) → flagged delinquent (§9.3, degraded tier). `ack_window_secs=null` opts out.

---

## 14. Outstanding Decisions

### Decided (cross-referenced above)
- ~~State store data shape~~ — §8 SQLite schema.
- ~~Reconciliation model (confirmed vs. observed, prompt flow)~~ — §8.3.
- ~~Core/wrapper architecture~~ — §4: core owns state + HTTP/JSON API; thin stateless wrappers; durable outbox; SQLite-only config.
- ~~Defaults/thresholds~~ — §13.
- ~~Threat model + verb authentication~~ — §10.1/§10.2: opportunistic-attacker model; auth-free loud triggering vs. authenticated dismissal; v1 stack = TLS (`tailscale serve`) + per-notification capability tokens + Tailscale device identity; OOB second factor and per-device keys deferred.
- ~~Verb delivery channels~~ — CLI (v1, primary) and ntfy action buttons (convenient path, safe under the §10.2 stack: TLS + single-use event-scoped tokens + tailnet-only). Web form later (same API). Email-reply parsing rejected.
- ~~Account system / multi-user~~ — §10.3: single-tenant by deployment, so access auth (§10.2) is complete; no identity/accounts. One daemon shared secret presented by front ends; web GUI "login" is secret-presentation, not user accounts.
- ~~Bootstrap / onboarding~~ — §5: two-level readiness, daemon node-connection bootstrap via env+CLI `init`, per-wallet `arming_state` machine, explicit balance-confirmation arming gate, loud not-armed visibility, restart ≠ bootstrap.
- ~~Failure-mode policy~~ — §7.1 (per-failure behavior) + §9.1 (three notification tiers: alarm / degraded / heartbeat, each with configurable cadence; recheck vs. re-notify; recovery notice; all-channels-down via heartbeat silence).
- ~~API endpoint list~~ — §11.1: hybrid REST + action paths; long-poll outbox; shared-secret auth with capability token on acknowledge; `init` as CLI not HTTP.
- ~~Detection approach / UTXO tracking~~ — §6: Core/Knots is source of truth for the watched set (owns xpub, derivation, UTXO set, reorgs); daemon is pull-shaped (ZMQ triggers, Core answers ownership) and stores no xpub/descriptor — a data-minimization choice that shrinks DB-leak blast radius.
- ~~State transition tables~~ — §8.6: all four state machines (wallet arming incl. `blinded`, event confirmation, event resolution, pre-auth) with triggers + side-effects; six edge cases resolved.
- ~~Payload conventions~~ — §11.2: snake_case, integer sats, ISO-8601 on the wire, stable error codes, async returns resource-with-state, single-object detail, responses structurally exclude xpub/descriptor.
- ~~Core/client boundary~~ — §4: channel-blind core; two client roles (egress notifiers = outbox-only, bidirectional control surfaces = verbs+query); role-scoped credentials (§10.2); outbox payload as the named interface contract; conformance rule; Option A channel management (core never models a channel; health via `delivered_by`). Notifier config/discovery = future seam above the core (§4.1), App Store excluded.
- ~~Architectural philosophy~~ — §4.0: three program types (core / notifier / control surface) + three published contracts (outbox, control, config); escalation split (core across notifiers, notifier within itself); §4.1 staged roadmap (Stage 0–3) + deployable-unit topology.
- ~~Fan-out delivery + notifier health~~ — §8 (`notifier`, `notifier_tier`, `outbox_delivery`), §9.2/§9.3: dispatch by tier; fan-out (not claim) so notifiers never block each other; two-stage ack (received→delivered); notifier registers identity + tier subscription + ack-window; core flags delinquent/stalled notifiers *by name* (degraded tier + heartbeat); explicit-null ack-window opts out; no rerouting/cadence-change to cover a user's coverage gap.
- ~~Per-field config mutability~~ — §4.1 Stage-2 note: notifier advertises each config field as read-only or read-write to control surfaces (e.g. a security notifier's tier subscription is read-only so a UI can't mute it).

### Must decide (has design weight)
- *(none remaining — all load-bearing decisions are settled; remaining items are minor/policy, future, or parked)*

### Minor / policy
- Whether to log benign *authorized* spends as events too (audit trail) or only alarms — config flag (§8.4); pick a default.
- Multi-wallet labeling UX wording (schema supports it via `wallet`).

### Future
- Self-hosted web GUI (wrapper over the API).
- Additional notification channels (Signal/Telegram/Matrix wrappers).
- User-facing scan-progress query (explicitly *not* MVP; `observed_status` is for heartbeat coordination, not this).

### Parked
- StartOS/Start9 packaging. If revisited: thin wrapper over the host-agnostic core; verify SDK version + whether the Start9 Bitcoin package exposes ZMQ to dependents, else fall back to RPC polling.
