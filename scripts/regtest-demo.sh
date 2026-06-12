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
bcli() { $COMPOSE exec -T bitcoind bitcoin-cli -regtest "$@"; }
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

step "human-in-the-loop: confirm the observed balance -> ARMED (never auto-arms)"
OBS=$(capi GET /wallets/$WID | jq -r .observed_at)
capi POST /wallets/$WID/balance/confirm "{\"observed_at\":\"$OBS\"}" | jq '{arming_state, reconciled_sat}'
capi GET /status | jq '{armed, not_protected}'

step "spend a watched UTXO -> ALARM"
SPEND_ADDR=$(bcli -rpcwallet=spender getnewaddress)
bcli -rpcwallet=coldsrc sendtoaddress "$SPEND_ADDR" 0.2 >/dev/null
sleep 5   # zmq rawtx -> detection pass
EV=$(capi GET '/events?resolution=firing' | jq '.[0]')
echo "$EV" | jq '{id, resolution, amount_sat, txid: .spending_txid}'
[ "$(echo "$EV" | jq -r .resolution)" = firing ] || { echo "FAIL: no firing event"; exit 1; }

step "the alarm reached ntfy (channel delivery, fan-out)"
curl -sf "$NTFY/json?poll=1" | jq -r '.title? // empty' | head -3

step "acknowledge with the single-use capability token -> silenced"
EID=$(echo "$EV" | jq -r .id)
# the token rides only in the alarm payload (events JSON never echoes it);
# control-scope may read the outbox for display, which is where we fish it out
TOKEN=$(capi GET '/outbox?tier=alarm' | jq -r ".[] | select(.payload.event_id==$EID) | .payload.capability_token" | head -1)
capi POST "/events/$EID/acknowledge" "{\"capability_token\":\"$TOKEN\"}" | jq '{id, resolution}'

step "pre-authorize the NEXT spend -> no alarm"
UTXO=$(bcli -rpcwallet=coldsrc listunspent | jq '.[0]')
TXID=$(echo "$UTXO" | jq -r .txid); VOUT=$(echo "$UTXO" | jq -r .vout)
capi POST /preauths "{\"wallet_id\":$WID,\"txid\":\"$TXID\",\"vout\":$VOUT,\"note\":\"planned move\"}" | jq '{id, status}'
bcli -rpcwallet=coldsrc sendtoaddress "$SPEND_ADDR" 0.1 >/dev/null
sleep 5
capi GET '/events?resolution=auto_resolved' | jq '.[0] | {id, resolution, authorized}'

step "done — tear down with: $COMPOSE down -v"
