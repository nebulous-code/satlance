#!/usr/bin/env bash
# Sat Lance — signet soak preflight.
#
# Proves both alert channels are configured correctly BEFORE you arm anything.
# A silently broken channel is the failure this project exists to prevent, and
# both of these paths have historically been the ones that break:
#   - the ntfy alarm (R1: a non-ASCII title made it undeliverable, forever)
#   - the SMTP escalation path (never executed until now)
#
# Run it once after filling in deploy/.env, and again any time you change
# credentials. It sends one real push and one real email.
#
# Usage: ./scripts/signet-preflight.sh
set -uo pipefail
cd "$(dirname "$0")/.."

COMPOSE="docker compose -f deploy/docker-compose.signet.yml --env-file deploy/.env"
PASS=0; FAIL=0

hdr()  { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
ok()   { printf '\033[1;32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '\033[1;31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }

[ -f deploy/.env ] || { echo "FAIL: deploy/.env not found — cp deploy/.env.example deploy/.env"; exit 1; }
# shellcheck disable=SC1091
set -a; . ./deploy/.env; set +a

for v in NTFY_URL MAIL_FROM MAIL_TO ZOHO_USER ZOHO_PASS; do
  [ -n "${!v:-}" ] || { echo "FAIL: $v is empty in deploy/.env"; exit 1; }
done

TOPIC="${NTFY_URL##*/}"
# The notifier reaches ntfy via the host gateway; from here it is loopback.
HOST_NTFY="$(printf '%s' "$NTFY_URL" | sed 's#//host\.docker\.internal#//127.0.0.1#')"
MARKER="preflight"

hdr "ntfy — topic $TOPIC"
# Posted from a container on the compose network with the same host-gateway
# mapping the notifier uses, so this exercises the real path, not a shortcut.
if $COMPOSE --profile preflight run --rm preflight 2>&1 | grep -q PREFLIGHT_NTFY_POSTED; then
  ok "push accepted by the server"
else
  bad "could not POST to $NTFY_URL from inside the compose network"
  echo "      check: is your ntfy container up, and does host.docker.internal resolve?"
fi

# Accepting a POST is not the same as storing it. Read it back off your server.
sleep 2
if curl -sf "${HOST_NTFY}/json?poll=1&since=all" 2>/dev/null | grep -q "$MARKER"; then
  ok "message read back off your ntfy instance (UTF-8 title survived)"
  echo "      -> confirm it also arrived on your phone before trusting it"
else
  bad "message was not readable back from ${HOST_NTFY}"
  echo "      the POST may have gone somewhere other than the instance on :8888"
fi

hdr "smtp relay -> Zoho"
$COMPOSE up -d relay >/dev/null 2>&1
# Give postfix a moment to finish starting before handing it mail.
for _ in $(seq 1 15); do
  $COMPOSE exec -T relay sh -c 'postfix status' >/dev/null 2>&1 && break
  sleep 2
done

STAMP="satlance-preflight-$(date +%s)"
if $COMPOSE exec -T relay sh -c \
     "printf 'Subject: Sat Lance preflight (%s)\nFrom: %s\nTo: %s\n\nIf this reached your inbox, the relay owns TLS to Zoho correctly and the SMTP escalation path is live.\n' '$STAMP' '$MAIL_FROM' '$MAIL_TO' | sendmail -f '$MAIL_FROM' '$MAIL_TO'" 2>/dev/null; then
  ok "mail queued with the relay"
else
  bad "sendmail rejected the message inside the relay container"
fi

# sendmail exiting 0 only means QUEUED. Postfix delivers asynchronously, so
# reporting success here would be exactly the kind of false green this whole
# project is organised against. Wait for the actual delivery status.
echo "      waiting for postfix to report a delivery status (up to 60s)..."
STATUS=""
for _ in $(seq 1 30); do
  LOG=$($COMPOSE logs --since 5m relay 2>/dev/null | grep -E 'status=(sent|bounced|deferred)' | tail -1)
  case "$LOG" in
    *status=sent*)     STATUS=sent;     break ;;
    *status=bounced*)  STATUS=bounced;  break ;;
    *status=deferred*) STATUS=deferred; break ;;
  esac
  sleep 2
done

case "$STATUS" in
  sent)     ok "Zoho accepted the message (status=sent)"
            echo "      -> confirm it landed in $MAIL_TO, not a spam folder" ;;
  bounced)  bad "Zoho rejected it (status=bounced) — usually a bad ZOHO_USER/ZOHO_PASS or a From: address the account may not send as"
            $COMPOSE logs --since 5m relay | grep -E 'status=bounced' | tail -3 ;;
  deferred) bad "delivery deferred — relay could not complete TLS/auth to smtp.zoho.com:587"
            $COMPOSE logs --since 5m relay | grep -E 'status=deferred' | tail -3 ;;
  *)        bad "no delivery status within 60s — inspect: $COMPOSE logs relay" ;;
esac

hdr "result"
printf '%d passed, %d failed\n' "$PASS" "$FAIL"
if [ "$FAIL" -eq 0 ]; then
  echo "Both channels configured. Neither is verified until you have seen the"
  echo "push on your phone and the mail in your inbox — go look before arming."
  exit 0
fi
echo "Fix the failures above before arming anything. An armed wallet with a"
echo "dead alert channel is the exact state this tool exists to prevent."
exit 1
