# Signet soak — runbook

The regtest harness proves the mechanism in two minutes. The signet soak proves the thing regtest structurally cannot: that Sat 'Lance runs **unattended for days against a real chain** without lying to you and without crying wolf.

Signet has ~10 minute blocks like mainnet, genuine reorgs, a real mempool, and worthless coins. It is the last stop before real funds.

## Why this stack is persistent

`deploy/docker-compose.regtest.yml` is disposable and `down -v` is part of its contract. This one is the opposite. Continuity **is** the test — the heartbeat schedule is persisted across restarts specifically so a daemon restart does not reset the cadence, and that only gets proven by restarting it on day three and watching the cadence hold.

**Never run `down -v` against this stack.** Use `stop` and `start`.

## What signet catches that regtest cannot

- **A dead ZMQ trigger path.** The `zmq_silent` degraded condition is suppressed when `network == "regtest"`, which is exactly why R5 survived every regtest run — the harness could never notice. Signet is the first environment where a broken trigger path raises a condition.
- **Reorgs.** They happen on their own schedule. Nothing to script.
- **The vanish and resurrection path (D2)** against a mempool that behaves like a mempool.
- **False alarms over a long window.** The only question that matters for a tool you leave armed, and a two-minute run cannot answer it.
- **Restart resilience** across many heartbeat cycles.

## Before you start

1. `cp deploy/.env.example deploy/.env` and fill it in. It holds your Zoho password; it is gitignored.
2. A Zoho **app-specific password**, not your account password.
3. Somewhere to get signet coins (any public signet faucet).

Ports used: `127.0.0.1:8331` for the control API. The regtest harness owns 8330; nothing else collides with your running services.

## 1. Bring it up

```
docker compose -f deploy/docker-compose.signet.yml --env-file deploy/.env up -d --build
```

The node starts syncing signet. `satlanced init` will **refuse** until the node is past IBD — that is correct behaviour, not a failure, and the container retries every 30 seconds. Signet is small; expect sync in hours, not days.

Watch it get there:

```
docker compose -f deploy/docker-compose.signet.yml logs -f core
```

You are waiting for `ZMQ confirmed publishing` followed by the daemon starting its duty cycle.

## 2. Preflight both alert channels

Run this **before arming anything**. It sends one real push and one real email, and it is the only thing standing between you and an armed wallet whose alerts go nowhere.

```
./scripts/signet-preflight.sh
```

It checks four things:

1. The push is accepted by your ntfy server, posted from inside the compose network using the same host-gateway path the notifier uses — not a shortcut from the host.
2. The message reads back off your instance. Accepting a POST is not the same as storing it.
3. Mail is queued with the relay.
4. **Postfix reports `status=sent`.** `sendmail` exiting 0 only means *queued* — postfix delivers asynchronously, so stopping there would report success for mail that later bounces. The script waits up to 60s for a real delivery status and tells you which of `sent` / `bounced` / `deferred` you got.

The test push deliberately carries a warning sign and an em dash. R1 was an alarm that could never be delivered because its title was non-ASCII while every other message was fine — so every casual test looked green while the one message that mattered failed forever. If this push arrives, that whole class is ruled out for your setup.

`bounced` almost always means bad `ZOHO_USER`/`ZOHO_PASS`, or a `From:` address the account is not permitted to send as. `deferred` means the relay could not complete TLS or auth to `smtp.zoho.com:587`.

The script passing means *configured*. It is not verified until you have seen the push on your phone and the mail in your inbox. Go look.

Re-run it any time you change credentials. It is profile-gated so it never fires during a normal `up` — with `restart: unless-stopped` on everything, an auto-firing test would push a message every time the host reboots.

**What the preflight does not prove.** It sends mail with `sendmail` inside the relay container, so it exercises postfix, STARTTLS, Zoho, and your mailbox — everything *downstream* of the notifier. `satlance-notifier-smtp` itself is bypassed: its SMTP conversation, outbox long-poll, two-stage ack, and message formatting have still never run. Passing here means a later mail failure is the notifier's fault and not your relay config, which is a useful narrowing, but it is not the same as the escalation path working. That is proven the first time a real alarm or degraded notice reaches your inbox *through the notifier* — see the delinquency scenario below.

## 3. Shorten the vanish timeout

`vanished_timeout_secs` defaults to **1209600 (14 days)**, matching Core's mempool expiry. You cannot exercise the D2 vanish-and-resurrect path inside a soak shorter than that, and D2 was the second-highest-severity finding in the original review.

There is no config verb yet (that is a tracked task), so this is a deliberate, documented DB poke — **with the daemon stopped**, so nothing is writing underneath it:

```
docker compose -f deploy/docker-compose.signet.yml stop core
docker run --rm -v satlance-signet-core:/d alpine sh -c \
  'apk add --no-cache sqlite >/dev/null && sqlite3 /d/satlance.db \
   "UPDATE config SET value='"'"'7200'"'"' WHERE key='"'"'vanished_timeout_secs'"'"';"'
docker compose -f deploy/docker-compose.signet.yml start core
```

Two hours makes the vanish cycle observable in an afternoon. Leave `heartbeat_secs` at its production value of 21600 (6h) — cadence holding at the real setting is part of what you are testing.

## 4. Arm a wallet

Create a signet wallet in your usual signing tool, fund it from a faucet, and export the **ranged external descriptor** (watch-only, xpub-based). Then:

```
export SATLANCE_API=http://127.0.0.1:8331
export SATLANCE_SECRET=$(docker compose -f deploy/docker-compose.signet.yml exec -T core cat /var/lib/satlance/control.secret)

satlance wallet add signet-cold --descriptor "<descriptor>"
satlance wallet list          # watch it walk registered -> importing -> awaiting
satlance status
```

When it reaches `awaiting`, **check the observed balance against your signing wallet before confirming.** A zero here means the import watched nothing, which is the looks-protected-but-isn't trap the whole tool exists to prevent. The confirmation is the last human defence and it is not a formality.

```
satlance balance confirm 1 --observed-at <observed_at from wallet list>
satlance status               # expect: armed, not_protected empty
```

## 5. The daily check

```
./scripts/signet-soak-check.sh
```

Roughly once a day. What you are looking for:

- `armed` and nothing under `not_protected`
- no unexplained `firing` events (you did not spend anything, so there should be none)
- no active degraded conditions you did not cause
- heartbeats arriving on your phone on cadence
- the reconciled balance still matching reality

## 6. Induced failures — run these partway through

The soak is not purely passive. Each of these has a specific expected behaviour, and each is a milestone done-when that has never been demonstrated outside a unit test.

**Node loss.** `docker compose -f deploy/docker-compose.signet.yml stop bitcoind`, wait past `node_grace_secs` (30s), then check. Expect the wallet to flip to **blinded**, show as NOT PROTECTED, and produce a degraded notice — *not* a theft alarm. Start the node again and expect an automatic recovery notice and a catch-up reconciliation back to `armed`.

**Notifier delinquency.** Stop `notifier-ntfy` for longer than its ack window (300s default). Expect it to be flagged delinquent **by name** on the degraded tier — which means the alert about the dead channel arrives via the *other* channel, your email. That is the escalation path proving itself.

**Escalation.** Leave an alarm unacknowledged (see below) and confirm it reaches both ntfy and email.

**Restart resilience.** `stop core` then `start core` on day two or three. The heartbeat cadence must survive; it must not fire an immediate heartbeat and reset the clock.

**A real spend.** Spend a watched UTXO from your signing tool. Expect a `firing` alarm within seconds, on both channels. Acknowledge it with `satlance event ack <id> <token>` and confirm escalation stops.

**A pre-authorized spend.** `satlance preauth add 1 <txid> <vout>`, then spend that outpoint. Expect `auto_resolved`, no alarm, and the baseline updated.

## What counts as a pass

- Multiple days armed with **zero false alarms**
- Every induced failure produced its expected notice, on the expected tier, by name
- Heartbeat cadence held across at least one daemon restart
- A real spend alarmed within seconds on both channels; a pre-authorized one did not alarm at all
- Nothing in the logs claiming a state that was not true

## Teardown

Only when the soak is genuinely finished:

```
docker compose -f deploy/docker-compose.signet.yml down
```

Add `-v` only if you want the node data and daemon state gone for good. There is no reason to hurry — the volumes are named `satlance-signet-node` and `satlance-signet-core` and they collide with nothing.
