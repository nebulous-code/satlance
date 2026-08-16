#!/usr/bin/env bash
# Sat Lance — signet soak daily check.
#
# One command that answers "is the soak still healthy, and has it lied to me
# yet." Read-only: it issues no verbs and changes no state.
#
# Usage: ./scripts/signet-soak-check.sh
set -uo pipefail
cd "$(dirname "$0")/.."

COMPOSE="docker compose -f deploy/docker-compose.signet.yml"
API=http://127.0.0.1:8331

hdr() { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
warn() { printf '\033[1;33mWARNING: %s\033[0m\n' "$*"; }

CTL=$($COMPOSE exec -T core cat /var/lib/satlance/control.secret 2>/dev/null) || {
  echo "FAIL: cannot read control.secret — is the core container up?"; exit 1; }
capi() { curl -sf -H "Authorization: Bearer $CTL" "$API$1"; }

hdr "containers"
$COMPOSE ps --format 'table {{.Service}}\t{{.Status}}'
# A restart policy makes a crash-loop look like "Up" on a quick glance.
RESTARTS=$($COMPOSE ps -q | xargs -r docker inspect -f '{{.Name}} {{.RestartCount}}' 2>/dev/null \
  | awk '$2 > 3 {print $1" ("$2" restarts)"}')
[ -n "$RESTARTS" ] && warn "containers restarting repeatedly: $RESTARTS"

hdr "node"
$COMPOSE exec -T bitcoind bitcoin-cli -signet -datadir="${SIGNET_NODE_DATADIR:-/data/.bitcoin}" \
  getblockchaininfo 2>/dev/null \
  | jq '{chain, blocks, headers, initialblockdownload, verificationprogress}' \
  || warn "could not query the node"

hdr "status (the honesty surface)"
capi /status | jq '{armed, not_protected, degraded}' || warn "status unreachable"

hdr "wallets"
capi /wallets | jq '[.[] | {id, label, arming_state, observed_sat, reconciled_sat, observed_status}]'

hdr "firing events — expect none unless you spent something"
FIRING=$(capi '/events?resolution=firing' | jq 'length')
capi '/events?resolution=firing' | jq '[.[] | {id, wallet_id, amount_sat, detected_at, confirmation}]'
[ "${FIRING:-0}" != "0" ] && warn "$FIRING alarm(s) FIRING — ack with: satlance event ack <id> <token>"

hdr "recent events (any resolution)"
capi '/events' | jq '[.[] | {id, resolution, authorized, amount_sat, detected_at}] | .[:10]'

hdr "notifier health"
# There is no GET /notifiers; delinquency surfaces as a degraded condition
# naming the notifier, which is the thing you actually want to see.
capi /status | jq '.degraded | map(select(.key | startswith("notifier_delinquent")))'

hdr "undelivered outbox rows — a silent channel is the failure mode that matters"
capi '/outbox' | jq '[.[] | select(.delivered_at == null) | {id, tier, created_at, title: .payload.title}]'

printf '\n'
