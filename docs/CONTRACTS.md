# CONTRACTS — the published HTTP surface

These contracts are the *only* coupling between deployable units. Anything
not written here is an implementation detail you must not depend on.

Conventions (design §11.2):

- JSON everywhere, `snake_case` keys.
- Amounts are **integer satoshis**. Never floats, never BTC strings.
- Timestamps are integer unix seconds in the DB, **ISO-8601 UTC strings on
  the wire** (`2026-06-12T14:30:00Z`).
- Async lifecycle changes return **202** with the current resource state.
- Errors are always `{"error": {"code": "<stable_code>", "message": "..."}}`.
  Codes are stable API; messages are not.

## Auth

`Authorization: Bearer <secret>` on everything except `GET /health`.

| Scope | Secret | May call |
|---|---|---|
| control | `control.secret` | everything below except notifier endpoints; may `GET /outbox` (read-only) |
| notifier | `notifier.secret` | `POST /notifiers/register`, `GET /outbox/pending`, `POST /outbox/{id}/received\|delivered` |

Wrong scope → `403 wrong_scope`. A notifier secret can never invoke a verb
that changes wallet/event state — enforced at the credential, not by
convention.

## Control surface

```
GET    /health
GET    /status
POST   /wallets                       {label, core_wallet_ref?, descriptor? | descriptors?}
GET    /wallets
GET    /wallets/{id}
DELETE /wallets/{id}
POST   /wallets/{id}/import           {descriptor | descriptors}
POST   /wallets/{id}/arm
POST   /wallets/{id}/disarm
POST   /wallets/{id}/rescan           (alias: /dispute)
POST   /wallets/{id}/balance/confirm  {observed_at | observed_at_unix}
POST   /wallets/{id}/balance/dispute
POST   /preauths                      {wallet_id, txid, vout, note?, ttl_secs?}
GET    /preauths
DELETE /preauths/{id}
GET    /events                        ?resolution=&confirmation=
GET    /events/{id}
POST   /events/{id}/acknowledge       {capability_token}
GET    /outbox                        ?tier=        (control, read-only display)
```

Notes:

- Wallet/preauth/event JSON **structurally cannot contain descriptors**
  (invariants 5–6). Descriptors exist only inside a request body on its way
  to the node.
- `balance/confirm` requires echoing the `observed_at` you actually saw;
  a mismatch is `409 stale_observation`. This is the never-auto-arm gate.
- `POST /preauths` for an outpoint that already has a pending preauth is
  `409 preauth_pending_exists`.
- `acknowledge` consumes the single-use capability token. Reuse →
  `403 capability_token_invalid`.

## Notifier surface

Register (idempotent on label):

```
POST /notifiers/register
{"label": "ntfy", "tiers": ["alarm","degraded","heartbeat"], "ack_window_secs": 300}
```

`ack_window_secs` absent → server default; explicit `null` → fire-and-forget
(never flagged delinquent).

Long-poll for work (returns early when something arrives, else after the
server's long-poll window):

```
GET /outbox/pending?label=ntfy
```

Response: array of delivery envelopes:

```
{
  "delivery_id": 12,
  "outbox_id": 7,
  "tier": "alarm",
  "kind": "spend_alarm",
  "payload": { ...see below... },
  "created_at": "2026-06-12T14:30:00Z",
  "state": "pending"
}
```

Two-stage ack, body `{"label": "<your label>"}`:

```
POST /outbox/{delivery_id}/received     "I have it; don't re-deliver"
POST /outbox/{delivery_id}/delivered    "the human-facing channel accepted it"
```

Miss your ack window → the core raises a `notifier_delinquent:<label>`
degraded condition (and clears it when you catch up).

## Message payload

Built by the core, channel-agnostic:

```
{
  "tier": "alarm" | "degraded" | "heartbeat",
  "kind": "spend_alarm" | "spend_alarm_repeat" | "degraded_notice" | ...,
  "title": "...",
  "body": "...",
  "created_at": "2026-06-12T14:30:00Z",
  // present when applicable:
  "wallet_label": "cold",
  "event_id": 3,
  "amount_sat": 18700000,
  "capability_token": "<single-use; alarm payloads only>",
  "ack_path": "/events/3/acknowledge"
}
```

The capability token appears **only** here (and in the control-scope
`GET /outbox` display). Event resources never echo it; the DB stores only
its hash.

## Tiers

- `alarm` — a watched UTXO moved without preauthorization, or the backstop
  found a discrepancy, or the disk stopped accepting writes. Wake-the-human.
- `degraded` — the guard's own health: node unreachable (armed wallets
  blinded), ZMQ silent, notifier delinquent, uncovered tier, preauth lapsed.
- `heartbeat` — periodic proof-of-life with armed / NOT PROTECTED summary.
