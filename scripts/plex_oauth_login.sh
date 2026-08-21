#!/usr/bin/env bash
# Obtain a Plex auth token via the official Plex OAuth PIN flow.
#
# The user opens the printed URL in a browser, signs in to Plex, and this
# script polls for the resulting token. Useful for:
#   - getting your own admin token
#   - having shared users (e.g. restricted accounts) obtain their own token
#     for testing, without them digging through browser storage
#
# Usage:
#   ./scripts/plex_oauth_login.sh [label]
#
# The token is printed once. Put it in .env yourself; this script never
# writes files.

set -euo pipefail

LABEL="${1:-replex}"
CLIENT_ID="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"

echo "Requesting PIN from plex.tv..."
PIN_RESPONSE=$(curl -sS --max-time 30 -X POST \
  -H "X-Plex-Client-Identifier: $CLIENT_ID" \
  -H "X-Plex-Product: replex-setup" \
  -H "Strong: true" \
  "https://plex.tv/api/v2/pins")

PIN_ID=$(printf '%s' "$PIN_RESPONSE" | grep -o '<id>[0-9]*</id>' | head -1 | tr -d 'a-z/<>')
CODE=$(printf '%s' "$PIN_RESPONSE" | grep -o '<code>[^<]*</code>' | head -1 | sed 's/<[^>]*>//g')

if [[ -z "$PIN_ID" || -z "$CODE" ]]; then
    echo "ERROR: could not parse PIN response" >&2
    exit 1
fi

AUTH_URL="https://app.plex.tv/auth#!?clientID=$CLIENT_ID&code=$CODE&context%5Bdevice%5D%5Bproduct%5D=replex"
echo ""
echo "==> Open this URL and sign in as the Plex account for '$LABEL':"
echo ""
echo "    $AUTH_URL"
echo ""
echo "Waiting for authentication (expires in ~5 minutes)..."

for i in $(seq 1 60); do
    sleep 5
    RESULT=$(curl -sS --max-time 30 \
        -H "X-Plex-Client-Identifier: $CLIENT_ID" \
        -H "Accept: application/json" \
        "https://plex.tv/api/v2/pins/$PIN_ID") || continue
    TOKEN=$(printf '%s' "$RESULT" | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get("authToken", ""))
except Exception:
    pass
')
    if [[ -n "$TOKEN" && "$TOKEN" != "null" ]]; then
        echo ""
        echo "Authenticated successfully."
        echo "Token for '$LABEL' (store in .env, do not share):"
        echo ""
        echo "    $TOKEN"
        echo ""
        echo "To verify it works:"
        echo "    curl -s \"https://plex.tv/api/v2/user?X-Plex-Token=<token>\" | head -c 200"
        exit 0
    fi
done

echo "Timed out waiting for authentication. Re-run to try again." >&2
exit 1
