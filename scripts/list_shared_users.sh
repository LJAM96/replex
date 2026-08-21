#!/usr/bin/env bash
# Phase 0 helper: list shared accounts (friends) with username + UUID so the
# REPLEX_USER_RESOLUTION_POLICIES entries can be prepared with stable ids.
#
# Usage:
#   PLEX_TOKEN=<admin token> ./list_shared_users.sh

set -euo pipefail

# Load .env from repo root if present.
# Precedence: existing env vars > .env. Empty values are skipped.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -f "$REPO_ROOT/.env" ]]; then
    while IFS='=' read -r k v; do
        [[ -z "$k" || "$k" =~ ^[[:space:]#] ]] && continue
        [[ -n "${!k:-}" || -z "$v" ]] && continue
        export "$k=$v"
    done < "$REPO_ROOT/.env"
fi

TOKEN="${PLEX_TOKEN:-${REPLEX_TOKEN:?Set REPLEX_TOKEN in .env or PLEX_TOKEN}}"

curl -sS "https://plex.tv/api/users?X-Plex-Token=$TOKEN&X-Plex-Client-Identifier=replex-phase0" \
| python3 -c '
import sys, xml.etree.ElementTree as ET
root = ET.fromstring(sys.stdin.read())
users = root.findall(".//User")
if not users:
    print("No shared users found.")
for u in users:
    print("username=%s  uuid=%s  title=%s" % (u.get("username"), u.get("id"), u.get("title")))
'
