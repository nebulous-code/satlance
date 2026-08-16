# GAPS — defect record

This file is the permanent record of defects found in Sat 'Lance: what was found, how it was resolved, and what is still open. `docs/STATUS.md` is the milestone-level view; this is the detail behind it.

Open defects come first. Everything below them is settled history, kept because it explains why parts of the code look the way they do.

## Open

### Multipath descriptors import half-watched (found 2026-08-15)

A descriptor in `<0;1>` multipath notation passes `begin_import`'s ranged check (it contains `*`) but never triggers the change-branch expansion, which only fires on a literal `/0/*`. The result is a single imported descriptor covering the receive branch alone.

Nothing about this looks wrong from the outside. `importdescriptors` returns `success: true`, and the initial observed balance is correct because first funding lands on a receive address — so the arming gate, the defence that catches the R3 class, passes cleanly. The gap opens on the first spend, when change moves to an unwatched branch and silently leaves the watched set. A later theft spending that change never alarms.

Measured against a live signet node with a real Sparrow-exported descriptor: importing the multipath form the way the daemon sends it gives `ismine: true` for a receive address and `ismine: false` for the first change address. Importing `/0/*` plus the synthesised `/1/*` watches both.

This matters more than its size suggests because **multipath is what Sparrow exports by default** — it is the most likely string a real user pastes. It is an R3-class failure and worse in one respect: R3 showed a zero balance a human would question, this shows a correct one.

Workaround: rewrite `/<0;1>/*` to `/0/*` before registering. Fix shape: expand multipath into separate branch descriptors as `/0/*` already is, with the correct `internal` flag per branch — or refuse it by name rather than half-importing it. A `getaddressinfo ismine` check on a derived change address after import would catch this whole class, and may be the better fix.

### The SMTP notifier has never executed

The binary builds and the crate is wired into the workspace, but nothing has ever run it. `deploy/docker-compose.regtest.yml` starts only the ntfy notifier, `deploy/Dockerfile.notifier-smtp` is unused by that harness, and `cargo test` reports zero tests for the crate. This is the same category as R1 — an unexercised seam that reads as green — and it is the escalation path, so it matters most exactly when the primary channel is already down.

The signet stack runs it for the first time. Note that preflighting the relay proves only the transport *downstream* of the notifier; the path is unproven until an alarm or degraded notice reaches a mailbox through the notifier itself.

### No runtime config surface

The `config` table receives its defaults at schema creation and nothing writes it afterwards — there is no `/config` endpoint and no CLI verb. Changing any tunable means a manual `sqlite3` UPDATE against a database the daemon holds open behind a single-writer mutex, which contradicts M3's "no manual DB pokes" done-when and is a poor answer for anyone self-hosting.

## Resolved: the June 2026 review

An independent review on 2026-06-12 read all four crates against the startup prompt and the milestones doc, ran `cargo test`, and executed `scripts/regtest-demo.sh`. It found 24 defects, concentrated in the seams that had never been executed: the ntfy delivery path, the automated descriptor import, and the acceptance harness itself. All were fixed in the same pass except the two noted below. The full findings are preserved further down.

| Finding | State | Where |
|---|---|---|
| R1 ntfy can't deliver the alarm | **fixed** | notifier publishes ntfy JSON (body, not headers); demo asserts the alarm title arrives |
| R2 import broken (checksum, label) | **fixed** | checksum stripped before branch expansion; no `label`; non-ranged refused (`descriptor_not_ranged`) |
| R3 blank wallet arms watching nothing | **fixed** | onboarding requires descriptors in the watch wallet; importing+blank+idle ⇒ `error`; demo asserts observed_sat == funded amount |
| R4 harness dies on bitcoind warm-up | **fixed** | compose retries `init` until the node verifies |
| R5 ZMQ bind-address endpoints | **fixed** | `SATLANCE_ZMQ_ENDPOINTS` override + 0.0.0.0→RPC-host rewrite; compose sets the service name |
| R6 demo cookie path | **fixed** | `bcli` passes `-datadir`; script assertions strengthened throughout |
| R7 M7 button auth (new, found during fixes) | **resolved by decision** | see below |
| D1 spending-tx dedup skip | **fixed** | per-outpoint dedup only; multi-outpoint test added |
| D2 vanish-by-age silences live spends | **fixed** | vanish needs conflicted/expunged/absent-from-mempool; resurrections reopen `auto_resolved` alarms (never `acknowledged`); decision table unit-tested |
| D3 backstop COUNT explanation | **fixed** | arithmetic: drop must not exceed summed evented spends (`drop_is_unexplained`, tested) |
| D4 tip-advance race | **fixed** | pre-scan tip persisted |
| O1 mainnet import timeout strands wallets | **fixed** | 10s import timeout = "rescan running"; `finish_pending_rescans` worker duty completes disputes |
| O2 mutex across RPC | **deferred** (user decision) | documented in STATUS sharp edges |
| O3 ZmqIdle tick storm | **fixed** | idle pokes gated; +2s echo pass after ZMQ activity closes the wallet-index race |
| O4 wallet delete | **fixed** | `409 wallet_has_history` (EDGE 4 decided: refuse; audit trail kept) |
| C1 ttl field mismatch | **fixed** | `ttl_secs` canonical, `ttl` alias; CLI updated |
| C2 control reads /outbox/pending | **fixed** | control passes only exact `GET /outbox` |
| C3 silent empty cap token | **fixed** | error log + `cap_token_key_missing` degraded condition |
| G1 STATUS renumbering | **fixed** | STATUS uses milestones-doc numbers |
| G2 missing EDGE walkthroughs | **fixed** | design §8.7 added (EDGE 4 = refuse-409) |
| G3 signet untested | **resolved** | signet stage built: `deploy/docker-compose.signet.yml`, runbook `docs/signet-soak.md`, preflight + daily-check scripts. Soak in progress |
| Nits (build deps, DB 0600, heartbeat persist, SMTP subject, ntfy Actions, unused import) | **fixed** | README, db.rs, worker.rs, notifier-smtp, notifier-ntfy |

### R7. The M7 phone-button ack cannot authenticate — resolved by decision

Found while fixing R1: the ntfy action button POSTs `/events/{id}/acknowledge` with the capability token, but that endpoint (correctly, per invariant 8) also requires the **control-scope secret** — which the phone doesn't have, and which must never ride a push payload, since that would hand the ntfy server the keys to every verb. The button as shipped has always returned `401`.

**Decided 2026-08-15.** The button stays but changes type: an ntfy `view` action deep-linking to the web UI's page for that event, rather than an `http` action POSTing the acknowledge. The human lands in a surface that already holds the control-scope secret and acknowledges there. Invariant 8 is preserved untouched, no new credential or scope is introduced, and a stale notification lands on the event's current state instead of firing a blind acknowledge at something already resolved. Implementation lands with M8, since it depends on a deep-linkable event view.

The two rejected options, recorded because the reasoning constrains future changes:

- **Capability-token-only auth.** The token travels through the ntfy server to reach the phone. Where ntfy runs with default open access, the topic name is the only thing protecting it — so this would make "can read my topic and reach the API" equal to "can silence my theft alarm." Topic names leak in ways secrets do not.
- **A third ack-only credential on the phone.** Honest two-factor, but it costs a new scope plus provisioning and rotation, and its real-world security reduces to possession of the phone, since the phone would hold both the credential and the token.

---

# The June 2026 review — findings as recorded

Kept verbatim. Resolutions are in the table above; the text below is the original record and describes the code as it was on 2026-06-12, not as it is now.

Method: the startup prompt (`docs/startup_docs/agent-startup-prompt.md`) and
milestones doc were used as the yardstick; every invariant and milestone
"done when" was checked against the code by reading all four crates, running
`cargo test`, and executing `scripts/regtest-demo.sh`. This file lists what
the spec promises that the implementation misses, plus implementation bugs
the spec didn't anticipate. STATUS.md's four *documented* deviations are not
re-listed except where the deviation has sharper consequences than STATUS
admits.

Verification results:

- `cargo test`: **36/36 pass** — but only after working around a missing
  `libsqlite3-dev` (rusqlite links the system library; see Nits).
- `scripts/regtest-demo.sh`: **fails as shipped** (it had never been run —
  STATUS.md says so honestly). Driving the same steps by hand verified the
  core premise end to end *and* surfaced five bugs the unit tests
  structurally cannot catch. See "Findings from executing the acceptance
  harness" — read that section first; R1–R3 are the highest-severity
  findings in this file.

Overall: the implementation is faithful to the design to an unusual degree.
The invariants that matter most (outpoint-identity detection, descriptor
never persisted/echoed, never auto-arm, role-scoped credentials, integer
sats, single-transaction state changes) check out in code, in tests, and
live on regtest. But the never-executed seams — the ntfy delivery path, the
automated descriptor import, the compose harness itself — are broken in
ways that include one full looks-protected-but-isn't failure (R3) and one
"the alarm specifically cannot be delivered" failure (R1).

---

## Findings from executing the acceptance harness (2026-06-12, Docker 29.5.2)

What was verified working, by hand-driving the demo steps: spend of a
watched UTXO → `firing` event within ~6s (via the RPC poll fallback —
see R5); correct outpoint identity and amount; acknowledge with the
capability token; token reuse rejected; pre-authorized spend →
`auto_resolved`, pre-auth `consumed`, zero alarm; heartbeat / balance
prompts delivered to ntfy; status honesty (`NOT PROTECTED` lines, armed
list). The detection heart of the tool is real.

### R1. The spend alarm — and only the spend alarm — cannot be delivered via ntfy

`notifier-ntfy` passes the payload title as an HTTP `Title:` header. HTTP
headers are ASCII; the **alarm title is the only payload in the system
containing non-ASCII** (`⚠ UNAUTHORIZED SPEND — cold`: emoji + em-dash),
so ureq rejects it client-side before sending:

```
ntfy delivery 3 failed: Bad Header: invalid header 'Title: ⚠ UNAUTHORIZED SPEND — cold'
```

Heartbeats, balance prompts, degraded notices, authorized-spend notes —
all ASCII — deliver fine, so every casual test looks green while the one
message the tool exists for fails on every retry, forever. The outbox
machinery behaved correctly around it (row never `delivered`, retried
each poll; the delinquency sweep would flag ntfy by name once the 300s
window lapses) — but with ntfy as your only alarm channel, the phone
never buzzes. Fix: publish to ntfy with a JSON body (UTF-8 clean) instead
of headers, or strip/encode non-ASCII in header values. Also: M2's "done
when" (push reaches a real device) is therefore **not met** for the alarm
tier, despite STATUS marking the outbox milestone done.

### R2. Automated descriptor import is broken twice over (never worked against the pinned node)

`arming.rs begin_import`:

1. The change-branch expansion synthesizes `…/1/*` by string-replacing
   inside the receive descriptor **but keeps the original checksum**, so
   `getdescriptorinfo` rejects it (`-5 Provided checksum 'a793awac' does
   not match computed checksum 'v2qsqmdq'`) and the whole import 502s.
2. Even with a valid checksum, the import request includes `"label": ""`,
   which Bitcoin Core 28.x rejects for ranged descriptors
   (`-8 Ranged descriptors should not have a label`) — against the exact
   `bitcoin/bitcoin:28.1` image the compose file pins.

Fix: strip the checksum before mutating the descriptor and let
`getdescriptorinfo` re-canonicalize (it already does), and omit `label`
for ranged descriptors.

### R3. A failed import arms a wallet that watches nothing (the cardinal sin, reached in practice)

The R2 failure happens **before** `set_state(importing)`, after the blank
Core wallet was already created. Sequence observed live:

1. `POST /wallets {descriptor}` → begin_import creates blank
   `satlance-cold`, fails at getdescriptorinfo → 502; wallet row stays
   `registered`; blank Core wallet left behind (no cleanup).
2. `poll_onboarding` sees `satlance-cold` loaded in Core → logs
   *"detected in Core (by-hand path)"* → `importing`.
3. The blank wallet isn't scanning → "scan complete" → balance **0** →
   `awaiting`.
4. One balance confirm later the wallet is **`armed`, watching zero
   descriptors**, while the real cold funds move unwatched. Heartbeats
   forever report "0.0 BTC reconciled, armed".

The explicit balance confirmation is the surviving defense (a human should
balk at 0 when Sparrow says 0.5) — but invariant 7's *"configured to watch
AND currently able to"* is violated: there is nothing configured to watch.
Fix shape: `poll_onboarding` must not treat a descriptorless wallet as a
completed by-hand import (check `listdescriptors` count > 0); and a failed
automated import should either clean up the blank wallet or park the row
in `error`, not `registered`.

### R4. The compose harness can't survive bitcoind's RPC warm-up

`deploy/docker-compose.regtest.yml` runs `satlanced init` exactly once,
gated only on the cookie file existing. bitcoind accepts connections
before it finishes warming (`-28 Loading P2P addresses…`), `init`
correctly refuses (M0 behavior), `satlanced run` then exits (no DB), the
container dies, and `regtest-demo.sh` polls `/health` forever. Reproduced
on the first boot. The init-refusal is right; the harness needs a retry
loop around init (treat `-28` as "wait", as the design's §7 startup-grace
note already prescribes for the daemon proper).

### R5. ZMQ endpoints are stored as the node's *bind* addresses — the trigger path is dead in any cross-host deploy

`init` persists what `getzmqnotifications` returns — `tcp://0.0.0.0:28332`,
the node's *listen* address. The subscriber then "connects" to `0.0.0.0`
from inside the **core's** container, i.e. to itself; no message ever
arrives. In the repo's own compose topology the ZMQ trigger path is
silently nonfunctional — detection worked in the demo only because the
worker's poll fallback runs every few seconds anyway. Worse, the
`zmq_silent` degraded condition is suppressed on regtest (`network !=
"regtest"` guard), so the harness can never notice. On a mainnet docker
deploy this means permanent "slow mode". Fix: allow an explicit
`SATLANCE_ZMQ_ENDPOINTS` override (and/or rewrite `0.0.0.0`/`127.0.0.1` to
the RPC URL's host at init).

### R6. `regtest-demo.sh` cookie path is wrong for the pinned image

`bcli()` execs `bitcoin-cli -regtest` without `-datadir`; in
`bitcoin/bitcoin:28.1` the daemon runs as user `bitcoin` with datadir
`/home/bitcoin/.bitcoin`, while `exec` lands as root looking in
`/root/.bitcoin` → *"Could not locate RPC credentials"*. Every `bcli`
call needs `-datadir=/home/bitcoin/.bitcoin` (or `exec -u bitcoin`).

---

## Detection correctness (the "silent false negative" class)

The startup prompt names a real spend that never alarms as the worst
possible failure. These three are exactly that class.

### D1. A spending tx is skipped entirely once *any* event references it

`detect.rs` `process_candidate_spend` (≈line 117) early-returns if any
`event` row exists with that `spending_txid` — globally, across wallets:

```sql
SELECT COUNT(*) FROM event WHERE spending_txid=?1
```

Two false-negative scenarios follow:

1. **Cross-wallet spend.** One transaction consumes UTXOs from two watched
   wallets (a consolidation, or a thief draining everything at once).
   Whichever wallet scans first records its outpoints; the second wallet's
   scan sees "already evented" and returns — the second wallet's spent
   outpoints never get event rows, never alarm, and its baseline drop later
   reads as "explained" if any other event exists (see D3).
2. **Crash between outpoints.** One tx spends two outpoints of the same
   wallet. Each `record_spend` is its own transaction; a crash between them
   means the survivor blocks re-processing of the candidate forever.

Fix shape: dedup must be per `(spent_txid, spent_vout, spending_txid)` —
which `idx_event_dedup` already enforces at insert — so the early-exit
short-circuit can simply be removed (or scoped per wallet + made
outpoint-complete). The pass is already idempotent without it.

### D2. `vanished` doesn't check the mempool, and vanished events are never re-tracked

`update_confirmations` (`detect.rs` ≈line 411) flips a `seen` event to
`vanished` purely on age: `confirmations == 0` and `detected_at` older than
14 days — even when `gettransaction` still returns the tx, i.e. the wallet
still considers it live in the mempool. Then:

- `vanished` auto-resolves the firing alarm (EDGE 6 — correct when the tx
  is truly gone), and
- the tracking query excludes `vanished` rows forever, and
- the dedup index blocks a fresh event for the same spend.

So an attacker who keeps a low-fee spend alive in the mempool past 14 days
(trivial: rebroadcast) gets the alarm auto-silenced, and when the tx
finally confirms, **nothing fires and nothing tracks it**. Exotic, but it
is a complete silence path for a real theft.

Fix shape: only transition to `vanished` when the node no longer knows the
tx (`-5`) or reports it conflicted (`confirmations < 0`); and let the
confirmation tracker re-open (`vanished → seen/confirmed`) if a vanished
spend reappears — the design's own §8.1 allows `vanished` only for
"dropped/replaced, never confirmed".

### D3. Backstop "explained" check is one COUNT, any event counts

STATUS.md documents the backstop simplification, but understates the
residual hole: `reconcile.rs backstop_check` treats a balance drop as
"explained" if **any** event row exists since the anchor —

```sql
SELECT COUNT(*) FROM event WHERE wallet_id=?1 AND detected_at>=?2
```

A wallet with one detected 0.01 BTC spend since the anchor can lose 5 BTC
to an undetected spend (e.g. via D1) and the drop reads "explained" —
prompt, not alarm. The backstop exists precisely to catch what the
real-time detector missed, so the explanation should at least be
arithmetic: sum of evented `amount_sat` since anchor vs. the observed
delta, alarm on shortfall. That's still far short of the spec's per-UTXO
comparison but closes the worst of it cheaply.

### D4. Block-position advance race (narrow)

`detection_pass` calls `listsinceblock` per wallet, *then* reads
`getbestblockhash` and persists it as the new scan floor. A block mined
between those two calls — containing a watched spend that never crossed
our mempool view — lands below the new floor and is never listed again.
The window is sub-second and the (D3) backstop is the catch-all, but the
fix is free: capture the tip *before* `listsinceblock` and persist that.

---

## Operational / will bite on mainnet

### O1. Automated descriptor import will time out and strand wallets in `error` on mainnet

`importdescriptors` with `"timestamp": 0` makes Core run a full-history
rescan **synchronously inside the RPC call**. The RPC client (`rpc.rs`)
has a 120-second timeout. On regtest that returns instantly (which is why
the demo passes); on mainnet a deep rescan takes hours, so the call will
time out client-side, `begin_import` will mark the wallet `error`
(arming.rs EDGE-1 path), and a retry hits Core's "wallet is currently
rescanning" error. Meanwhile Core is actually scanning fine — the daemon's
`error` state lies, which is the cardinal sin the design organizes itself
around. `bump_and_rescan` (dispute path) has the same problem.

Fix shape: treat a transport timeout on `importdescriptors` as "import
started, rescan running" (stay in `importing` — `poll_onboarding` already
watches `getwalletinfo.scanning` and would complete the flow correctly),
or call `importdescriptors` with a recent timestamp + explicit
`rescanblockchain` in the background. Same for the dispute rescan.

### O2. The worker and API share one mutex; one slow RPC freezes both

Every API handler and the entire worker duty cycle hold the same
`Mutex<Connection>` across RPC calls (up to the 120s timeout each). A
stalled node call in the worker blocks every API request (except
`/health`) including the CLI and notifier acks for that duration. Fine at
regtest scale; on mainnet during a rescan this reads as "daemon hung".
Worth at minimum a note; properly, take RPC calls outside the lock (read
inputs → drop lock → call node → re-lock to write).

### O3. `ZmqIdle` defeats the 30s tick floor

The ZMQ thread sends `ZmqIdle` every 5s of quiet (its recv timeout), and
the worker runs the **full duty cycle on every wake** regardless of trigger
type. So the claimed "30s tick floor" (STATUS M3 note) is actually a ~5s
detection-pass cadence whenever the chain is quiet — extra
`listsinceblock`/`gettransaction` chatter, no correctness impact. Either
gate duties on `Zmq|ZmqGap|Nudge|Timeout` or drop `ZmqIdle` (the worker
already tracks silence by timestamp).

### O4. `DELETE /wallets/{id}` breaks once the wallet has history

`foreign_keys=ON` plus child rows (`event`, `preauth`, `outbox`) means the
bare `DELETE FROM wallet` fails the FK constraint; the handler maps any
failure to `unwrap_or(0)` → **404 "no such wallet"** for a wallet that
plainly exists. Practically: a wallet that has ever fired an event cannot
be removed, and the error message gaslights. The design's §8.6 references
an ‹EDGE 4› policy for exactly this (open events/pre-auths on delete), but
the walkthrough is missing from the doc (see Docs) and the implementation
never made a choice. Needs a decision: cascade, soft-delete, or refuse
with an honest `409 wallet_has_history`.

---

## Contract / API mismatches

### C1. Preauth TTL field name: `ttl_secs` (CONTRACTS.md) vs `ttl` (code, CLI)

`CONTRACTS.md` advertises `POST /preauths {…, ttl_secs?}`; `api.rs` reads
`body["ttl"]` and the CLI sends `ttl`. Anyone building against the
published contract gets their TTL silently ignored (falls back to the 24h
default — at least it fails toward expiry). Pick one (the design §11.1
says `ttl?`; the convention elsewhere is `_secs`) and fix doc or code.

### C2. Control scope can long-poll `/outbox/pending`

The scope rule in `api.rs handle()` admits control-scope GETs on all of
`/outbox/*`, which includes `pending` — and the handler updates the
notifier's `last_seen_at` as a side effect. CONTRACTS.md grants control
only the `GET /outbox` display read. Harmless today (`last_seen_at` is a
liveness aid, delinquency runs on delivery rows), but it lets a curious
control client smear a dead notifier's liveness signal. Tighten the rule
to exactly `GET /outbox`.

### C3. Empty capability token on mint failure

`record_spend` does `mint_cap_token(...).unwrap_or_default()` — if the
token key is missing (corrupt meta), the alarm goes out with an empty
token and can never be acknowledged. That fails in the safe direction
(can't-dismiss, per invariant 11) but does so *silently*. Log it loudly
and/or surface a degraded condition so the operator knows ack is broken.

---

## Docs / process

### G1. STATUS.md renumbers the milestones

STATUS.md's table (M0 skeleton … M8 web UI) does not match
`startup_docs/btc-watch-daemon-milestones.md` (where M5 = pre-auth,
M6 = robustness, M7 = remote hardening). Pre-authorization — milestone
M5, "arguably where it becomes a tool you'd actually keep running" — has
no line of its own in STATUS, though it *is* fully implemented and
tested. Anyone auditing build-vs-plan has to re-derive the mapping.
STATUS should use the same numbering or carry an explicit mapping.

### G2. The design doc's ‹EDGE n› walkthroughs are referenced but absent

§8.6 says edge cases are "walked through below" and the tables cite
‹EDGE 1›–‹EDGE 6›, but no walkthrough section exists in the document. The
code cites the EDGE numbers in comments (so the *agent* inferred them from
the table side-effects), but EDGE 4 (wallet delete with open events) had
no table side-effect to infer from — and that's exactly the one that's
broken (O4). Worth restoring that section or deleting the references.

### G3. Test-environment progression stopped at regtest

The startup prompt's test strategy is regtest → signet → mainnet. Nothing
in the repo mentions signet (no config, no docs note). Regtest demo +
36 unit tests is a solid floor, but the signet soak — realistic timing,
reorgs, mempool behavior — is unstarted and undocumented as a to-do.

---

## Nits / sharp edges (noting, not urging)

- **Build requires system `libsqlite3-dev` and `libzmq`**, undocumented.
  This machine had neither dev package; `cargo test` fails at link with a
  bare `-lsqlite3` error. Either document the apt packages in the README
  or use `rusqlite`'s `bundled` feature.
- **DB file permissions**: secrets are written 0600, but `satlance.db`
  is created with default umask (typically world-readable). Design §10
  asks for tight permissions on the DB too. (DB contents are
  deliberately low-sensitivity — but the cap-token key lives in `meta`.)
- **Non-ranged descriptors can't import**: `begin_import` always sends
  `"range"`, which Core rejects for un-ranged descriptors (`addr(…)`,
  fixed-path keys). Only ranged `…/*` descriptors work. Probably fine for
  the cold-storage use case; worth an explicit error message.
- **Heartbeat timer is in-memory**: every daemon restart fires an
  immediate heartbeat and resets the cadence. Harmless (extra proof of
  life), but a crash-looping daemon becomes a heartbeat spammer rather
  than an obvious silence.
- **A `reorged` event whose tx sits at 0-conf never times out** — the
  vanish path only covers `seen`. It re-resolves if the tx confirms or
  the wallet drops it; otherwise it lingers in the tracking query forever.
- **SMTP subject takes the payload title unsanitized** — a wallet label
  containing CRLF could inject mail headers. Labels come from
  control-scope only (self-inflicted), but a one-line strip is cheap.
- **ntfy ack button uses the comma-separated `Actions` header** with an
  inline JSON body; safe today (single-key JSON, no comma), breaks if the
  body ever gains a second field. A quoted action string would be sturdier.
- **`balance_confirm_prompt` and confirmation updates ride tiers
  semantically loosely** — routine confirm prompts go out on `degraded`
  (renders as high-priority warning in ntfy) and confirmation updates on
  `heartbeat`. The spec never pinned tiers for these kinds; flagging as a
  UX choice to revisit, not a violation.

---

## The order they were fixed in

Recorded for the reasoning, not as outstanding work — all of these are resolved.

1. **R1** — alarm undeliverable via ntfy (the tool's whole job)
2. **R3 + R2** — automated import arms an empty watch set
3. **D1** — cross-wallet / partial-spend dedup skip (silent false negative)
4. **D2** — vanished-by-age silences a still-live spend permanently
5. **R5** — ZMQ dead in containerized deploys (latency + false "slow mode")
6. **O1** — mainnet import/rescan timeout strands wallets in `error`
7. **R4 + R6** — make the acceptance harness actually runnable in CI
8. **D3, D4, O4, C1** — backstop arithmetic, position race, wallet delete, TTL field

---

## What the pattern says

Every high-severity finding in this file, in both reviews, is the same shape: **a seam that had never been executed.** R1 was the alarm path, R2 and R3 the automated import, R4 through R6 the harness itself, and the multipath defect above is the descriptor form nobody had pasted yet. None of them were subtle logic errors; the unit tests were good and stayed green throughout.

The corollary is that reading code and adding tests does not find these. Running the thing against real infrastructure does — which is the entire argument for the signet soak, and for treating any unexecuted path as unproven no matter how obviously correct it looks.
