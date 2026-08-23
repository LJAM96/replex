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
