#!/usr/bin/env bash
# Phase 0 baseline capture for per-user resolution restrictions.
#
# Captures stock Replex responses (JSON + XML) for the endpoints affected by
# the resolution policy feature, so post-implementation behaviour can be
# regression-diffed against them.
#
# Usage:
#   REPLEX_BASE_URL=http://localhost:8000 PLEX_TOKEN=xxxx ./capture_baseline.sh
#   REPLEX_BASE_URL=... PLEX_TOKEN=xxxx ./capture_baseline.sh --verify
#
# Optional overrides:
#   MOVIE_RATING_KEY   rating key of a movie with BOTH 1080p and 4K versions
#   EPISODE_RATING_KEY rating key of an episode with both versions
#   SECTION_ID         library section id to browse
#
# With --verify, re-captures into a temp dir and diffs against the committed
# baseline (exit 1 on drift).

set -euo pipefail

# Load .env from repo root if present.
# Precedence: existing env vars > .env > defaults. Empty values are skipped,
# so passing SECTION_ID=... on the command line always wins over an empty
# template line in .env.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -f "$REPO_ROOT/.env" ]]; then
    while IFS='=' read -r k v; do
        [[ -z "$k" || "$k" =~ ^[[:space:]#] ]] && continue
        [[ -n "${!k:-}" || -z "$v" ]] && continue
        export "$k=$v"
    done < "$REPO_ROOT/.env"
fi

BASE="${REPLEX_BASE_URL:-${REPLEX_HOST:?Set REPLEX_HOST in .env or REPLEX_BASE_URL}}"
TOKEN="${PLEX_TOKEN:-${REPLEX_TOKEN:?Set REPLEX_TOKEN in .env or PLEX_TOKEN}}"
OUT_DIR="tests/mock/out/baseline"
MODE="${1:-capture}"

if [[ "$MODE" == "--verify" ]]; then
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT
    DEST="$WORK"
else
    mkdir -p "$OUT_DIR"
    DEST="$OUT_DIR"
fi

fetch() {
    # fetch <name> <path> [accept]
    local name="$1" path="$2" accept="${3:-application/json}"
    local file="$DEST/${name}.json"
    [[ "$accept" == *xml* ]] && file="$DEST/${name}.xml"
    local status
    status=$(curl -sS --connect-timeout 10 --max-time 180 -o "$file" -w '%{http_code}' \
        -H "Accept: $accept" \
        -H "X-Plex-Token: $TOKEN" \
        -H "X-Plex-Client-Identifier: replex-baseline-capture" \
        "${BASE}${path}")
    echo "$status $name" >&2
    if [[ "$status" != "200" ]]; then
        echo "WARN: $name returned HTTP $status (recorded anyway)" >&2
    fi
}

# --- discover rating keys / section id unless overridden -------------------
SECTION_ID="${SECTION_ID:-}"
MOVIE_RATING_KEY="${MOVIE_RATING_KEY:-}"
EPISODE_RATING_KEY="${EPISODE_RATING_KEY:-}"
if [[ -z "$SECTION_ID" ]]; then
    fetch "_probe_sections" "/library/sections"
    SECTION_ID=$(python3 - "$DEST/_probe_sections.json" <<'EOF'
import json, sys
c = json.load(open(sys.argv[1]))["MediaContainer"]
for d in c.get("Directory", []):
    if d.get("type") == "movie":
        print(d["key"]); break
EOF
)
fi

if [[ -z "$MOVIE_RATING_KEY" ]]; then
    fetch "_probe_section_content" "/library/sections/$SECTION_ID/all?X-Plex-Container-Size=20"
    MOVIE_RATING_KEY=$(python3 - "$DEST/_probe_section_content.json" <<'EOF'
import json, sys
c = json.load(open(sys.argv[1]))["MediaContainer"]
for m in c.get("Metadata", []):
    if m.get("type") == "movie" and len(m.get("Media", [])) >= 1:
        print(m["ratingKey"]); break
EOF
)
    echo "NOTE: auto-picked movie $MOVIE_RATING_KEY. Prefer a movie with BOTH 1080p+4K:" \
         "re-run with MOVIE_RATING_KEY=<key>" >&2
fi

if [[ -z "$EPISODE_RATING_KEY" ]]; then
    fetch "_probe_episodes" "/library/sections/$SECTION_ID/all?type=4&X-Plex-Container-Size=5"
    EPISODE_RATING_KEY=$(python3 - "$DEST/_probe_episodes.json" <<'EOF'
import json, sys
c = json.load(open(sys.argv[1]))["MediaContainer"]
for m in c.get("Metadata", []):
    if m.get("type") == "episode":
        print(m["ratingKey"]); break
EOF
)
fi

echo "== section=$SECTION_ID movie=$MOVIE_RATING_KEY episode=$EPISODE_RATING_KEY ==" >&2

# --- required baseline captures --------------------------------------------
fetch "library_sections"            "/library/sections"
fetch "library_sections_all"        "/library/sections/$SECTION_ID/all?X-Plex-Container-Size=50"
fetch "movie_detail"                "/library/metadata/$MOVIE_RATING_KEY"
fetch "episode_detail"              "/library/metadata/$EPISODE_RATING_KEY"
fetch "hubs_promoted"               "/hubs/promoted?pinnedContentDirectoryID=$SECTION_ID&count=12"
fetch "hubs_home"                   "/hubs/home?count=12"

# XML variants (some clients request XML)
fetch "library_sections"            "/library/sections"                 "application/xml"
fetch "movie_detail"                "/library/metadata/$MOVIE_RATING_KEY" "application/xml"
fetch "episode_detail"              "/library/metadata/$EPISODE_RATING_KEY" "application/xml"

# Playback decision + start templates.
# These exercise the version-selection path; query params mirror what Plex Web
# sends for direct play of a specific item.
DECISION_QS="path=%2Flibrary%2Fmetadata%2F$MOVIE_RATING_KEY&protocol=http&address=127.0.0.1&port=32400&hasMDE=1&hasPrefetch=1&directPlay=1&directStream=1&session=replex-baseline"
fetch "playback_decision" "/video/:/transcode/universal/decision?$DECISION_QS"
fetch "playback_start"    "/video/:/transcode/universal/start?$DECISION_QS"

# Explicit mediaIndex selection (the bypass path the policy must close)
fetch "playback_decision_mediaindex" "/video/:/transcode/universal/decision?$DECISION_QS&mediaIndex=1"

# Optional: a 4K-ONLY movie, for testing the hidden-item policy path
if [[ -n "${FOUR_K_MOVIE_RATING_KEY:-}" ]]; then
    fetch "movie_detail_4k_only" "/library/metadata/$FOUR_K_MOVIE_RATING_KEY"
    fetch "movie_detail_4k_only" "/library/metadata/$FOUR_K_MOVIE_RATING_KEY" "application/xml"
fi

rm -f "$DEST"/_probe_*.json

if [[ "$MODE" == "--verify" ]]; then
    # Normalize volatile attributes Plex changes on its own schedule
    # (library edit counters, timestamps) before comparing.
    normalize() {
        local src="$1" dst="$2"
        mkdir -p "$dst"
        while IFS= read -r f; do
            local rel="${f#$src/}"
            sed -E 's/"?(contentChangedAt|updatedAt|scannedAt|lastViewedAt|sessionKey)"?[:=]"?[0-9]+"?/\1=N/g' \
                "$f" > "$dst/$rel"
        done < <(find "$src" -type f)
    }
    NORM_BASE="$(mktemp -d)"
    NORM_NEW="$(mktemp -d)"
    trap 'rm -rf "$WORK" "$NORM_BASE" "$NORM_NEW"' EXIT
    normalize "$OUT_DIR" "$NORM_BASE"
    normalize "$DEST" "$NORM_NEW"

    # Only byte-compare stable fixtures. Hub/collection/browse responses are
    # inherently dynamic (trending rows, view counts); for those we only
    # require that they were captured successfully.
    STABLE="library_sections.json library_sections.xml movie_detail.json \
movie_detail.xml episode_detail.json episode_detail.xml \
movie_detail_4k_only.json movie_detail_4k_only.xml"
    FAIL=0
    for f in $STABLE; do
        if ! diff -q "$NORM_BASE/$f" "$NORM_NEW/$f" >/dev/null 2>&1; then
            echo "DRIFT: $f"
            diff "$NORM_BASE/$f" "$NORM_NEW/$f" | head -20
            FAIL=1
        fi
    done
    for f in $(cd "$OUT_DIR" && ls); do
        if [[ " $STABLE " != *" $f "* ]] && [[ ! -s "$NORM_NEW/$f" ]]; then
            echo "MISSING during verify: $f"
            FAIL=1
        fi
    done
    if [[ "$FAIL" == 0 ]]; then
        echo "OK: baseline matches (stable fixtures identical, volatile endpoints present)"
    else
        exit 1
    fi
else
    echo "Baseline written to $OUT_DIR ($(ls "$DEST" | wc -l) files)"
    echo "Re-run with --verify any time to regression-check stock behaviour."
fi
