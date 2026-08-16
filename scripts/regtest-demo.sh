#!/usr/bin/env bash
# Sat Surveillance — regtest demo / acceptance run.
#
# Drives the compose harness through the milestone done-when checks:
#   1. init + health
#   2. register a watch wallet by descriptor, import, balance-confirm, ARM
#   3. spend a watched UTXO         -> spend_alarm fires, ntfy receives it
#   4. acknowledge with the capability token -> escalation stops
#   5. pre-authorize an outpoint, spend it   -> auto_resolved, NO alarm
#
# Requires: docker compose, curl, jq.
set -euo pipefail
cd "$(dirname "$0")/.."

COMPOSE="docker compose -f deploy/docker-compose.regtest.yml"
API=http://127.0.0.1:8330
NTFY=http://127.0.0.1:8338/satsurv

step() { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
# -datadir: the bitcoin/bitcoin image runs the daemon as user 'bitcoin' with
# its data in /home/bitcoin/.bitcoin; exec lands as root, which would
# otherwise look for the cookie in /root/.bitcoin and fail.
bcli() { $COMPOSE exec -T bitcoind bitcoin-cli -regtest -datadir=/home/bitcoin/.bitcoin "$@"; }
capi() { # capi <method> <path> [json-body]
  local m=$1 p=$2 body=${3:-}
  if [ -n "$body" ]; then
    curl -sf -X "$m" -H "Authorization: Bearer $CTL" -H 'Content-Type: application/json' -d "$body" "$API$p"
  else
    curl -sf -X "$m" -H "Authorization: Bearer $CTL" "$API$p"
  fi
}

step "bring up the stack"
$COMPOSE up -d --build
until curl -sf "$API/health" >/dev/null 2>&1; do sleep 2; printf .; done
echo " core is up"
CTL=$($COMPOSE exec -T core cat /var/lib/satsurv/control.secret)

step "fund a regtest 'cold' wallet (stands in for your cold storage)"
bcli createwallet spender >/dev/null
bcli createwallet coldsrc >/dev/null
MINE=$(bcli -rpcwallet=spender getnewaddress)
bcli generatetoaddress 101 "$MINE" >/dev/null
COLD_ADDR=$(bcli -rpcwallet=coldsrc getnewaddress "" bech32)
bcli -rpcwallet=spender sendtoaddress "$COLD_ADDR" 0.5 >/dev/null
bcli generatetoaddress 1 "$MINE" >/dev/null
# the ranged external descriptor of the cold wallet — this is what you'd
# register from your real wallet (xpub-based, watch-only)
DESC=$(bcli -rpcwallet=coldsrc listdescriptors | jq -r '.descriptors[] | select(.internal==false and (.desc|startswith("wpkh"))) | .desc' | head -1)

step "register the wallet with satsurv (descriptor forwarded, never stored)"
capi POST /wallets "{\"label\":\"cold\",\"descriptor\":$(jq -Rn --arg d "$DESC" '$d')}" | jq .
WID=1

step "wait for import + scan -> 'awaiting' with an observed balance"
for i in $(seq 1 60); do
  STATE=$(capi GET /wallets/$WID | jq -r .arming_state)
  [ "$STATE" = awaiting ] && break
  sleep 2
done
capi GET /wallets/$WID | jq '{arming_state, observed_sat, observed_at}'
[ "$STATE" = awaiting ] || { echo "FAIL: never reached awaiting (state=$STATE)"; exit 1; }
# The observed balance must be the funded 0.5 BTC. A zero here means the
# descriptor import silently failed and we'd be arming an EMPTY watch set —
# the looks-protected-but-isn't trap this tool exists to prevent.
OBS_SAT=$(capi GET /wallets/$WID | jq -r .observed_sat)
[ "$OBS_SAT" = 50000000 ] || { echo "FAIL: observed_sat=$OBS_SAT, expected 50000000 — import did not watch the funds"; exit 1; }

step "human-in-the-loop: confirm the observed balance -> ARMED (never auto-arms)"
OBS=$(capi GET /wallets/$WID | jq -r .observed_at)
capi POST /wallets/$WID/balance/confirm "{\"observed_at\":\"$OBS\"}" | jq '{arming_state, reconciled_sat}'
capi GET /status | jq '{armed, not_protected}'

step "spend a watched UTXO -> ALARM"
SPEND_ADDR=$(bcli -rpcwallet=spender getnewaddress)
bcli -rpcwallet=coldsrc sendtoaddress "$SPEND_ADDR" 0.2 >/dev/null
EV=null
for i in $(seq 1 20); do   # zmq rawtx -> detection pass (30s tick is the backstop)
  EV=$(capi GET '/events?resolution=firing' | jq '.[0]')
  [ "$(echo "$EV" | jq -r .resolution)" = firing ] && break
  sleep 2
done
echo "$EV" | jq '{id, resolution, amount_sat, txid: .spending_txid}'
[ "$(echo "$EV" | jq -r .resolution)" = firing ] || { echo "FAIL: no firing event"; exit 1; }

step "the alarm reached ntfy (channel delivery, fan-out)"
# The ALARM specifically must arrive — heartbeats delivering while the alarm
# fails (e.g. non-ASCII title rejected at the transport) is the worst kind of
# green: give the notifier a few seconds to pull and deliver, then require it.
ALARM_SEEN=""
for i in $(seq 1 15); do
  if curl -sf "$NTFY/json?poll=1&since=all" | jq -r '.title? // empty' | grep -q "UNAUTHORIZED SPEND"; then
    ALARM_SEEN=1; break
  fi
  sleep 2
done
curl -sf "$NTFY/json?poll=1&since=all" | jq -r '.title? // empty' | tail -5
[ -n "$ALARM_SEEN" ] || { echo "FAIL: the spend alarm never reached ntfy"; exit 1; }

step "acknowledge with the single-use capability token -> silenced"
EID=$(echo "$EV" | jq -r .id)
# the token rides only in the alarm payload (events JSON never echoes it);
# control-scope may read the outbox for display, which is where we fish it out
TOKEN=$(capi GET '/outbox?tier=alarm' | jq -r ".[] | select(.payload.event_id==$EID) | .payload.capability_token" | head -1)
capi POST "/events/$EID/acknowledge" "{\"capability_token\":\"$TOKEN\"}" | jq '{id, resolution}'

step "pre-authorize the NEXT spend -> no alarm"
bcli generatetoaddress 1 "$MINE" >/dev/null   # confirm the change so listunspent sees it
UTXO=$(bcli -rpcwallet=coldsrc listunspent | jq '.[0]')
TXID=$(echo "$UTXO" | jq -r .txid); VOUT=$(echo "$UTXO" | jq -r .vout)
[ "$TXID" != null ] || { echo "FAIL: no spendable UTXO to pre-authorize"; exit 1; }
capi POST /preauths "{\"wallet_id\":$WID,\"txid\":\"$TXID\",\"vout\":$VOUT,\"note\":\"planned move\"}" | jq '{id, state}'
bcli -rpcwallet=coldsrc sendtoaddress "$SPEND_ADDR" 0.1 >/dev/null
RESOLVED=""
for i in $(seq 1 15); do
  N=$(capi GET '/events?resolution=auto_resolved' | jq 'length')
  [ "$N" -gt 0 ] && { RESOLVED=1; break; }
  sleep 2
done
capi GET '/events?resolution=auto_resolved' | jq '.[0] | {id, resolution, authorized}'
[ -n "$RESOLVED" ] || { echo "FAIL: pre-authorized spend did not auto-resolve"; exit 1; }
FIRING=$(capi GET '/events?resolution=firing' | jq 'length')
[ "$FIRING" = 0 ] || { echo "FAIL: pre-authorized spend still raised an alarm"; exit 1; }

step "done — tear down with: $COMPOSE down -v"
