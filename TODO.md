# Ideas / backlog

## Background hub cache warmer

**DONE 2026-08-23**: implemented as `hub_cache::spawn_warmer` (REPLEX_WARM_INTERVAL).
Canonical cache keys landed in the same change. Remaining idea: iterate existing
moka keys instead of rebuilding from /library/sections.

Keep both replex's payload cache and Plex Media Server's internal caches warm
by periodically fetching the hot hub endpoints in the background.

Why:
- PMS regenerates `/hubs/promoted` very slowly when its own cache is cold:
  measured 4s-53s on the live server (2026-08). replex's stale-while-revalidate
  layer hides this from clients, but background refreshes themselves would
  still pay that cost once per REPLEX_HUB_STALE_TTL window per key.
- A low-frequency timer (e.g. every cache_ttl/2) hitting each cached
  `hubcache:` key's path with the admin token would keep refreshes fast and
  make cold starts disappear entirely.

Design sketch:
- Reuse `hub_cache::fetch_hubs_payload` + the shared raw-payload cache.
- Iterate keys currently present in the payload cache (moka supports
  iteration) rather than hardcoding endpoints, so warmed set follows usage.
- Single-flight against `spawn_hub_refresh`'s inflight/rate-limit guards so
  warmer + on-demand refresh never double-fetch.
- Config knob: `REPLEX_WARM_INTERVAL` seconds, 0 = off. Default maybe 600s.
- Respect `REPLEX_CACHE_TTL=0` (caching disabled => warmer pointless).

Related edge case noticed along the way: Plexamp/LiveTV requests bypass the
normal hoop chain (`should_skip`), so their playback events don't trigger
hub invalidation. Webhook integration covers this if configured.

## Networking quirk: REPLEX_HOST must stay on the WAN path

Plex Media Server on this VPS (docker, linuxserver image) treats
loopback / docker-bridge / LAN-source connections as trusted-local and
ignores X-Plex-Token for them: `/library/sections` and other library
endpoints return an EMPTY container with no error. The tailscale IP
path 401s instead. Only connections that arrive via the public IP
DNAT hairpin (`http://65.109.70.216:42442`, ~90ms RTT) authenticate
tokens through plex.tv correctly.

Consequence: REPLEX_HOST must stay on the public-IP URL even though a
local path would be ~90x faster per call. The hub cache, SWR and the
background warmer exist to absorb that latency. If you ever see empty
hubs right after changing REPLEX_HOST to a local address, this is why.
