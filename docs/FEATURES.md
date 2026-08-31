# Feature behavior

This reference explains every active Replex feature, its trigger, and its interactions. See [Configuration](CONFIGURATION.md) for formats and defaults.

## Request processing order

Each request gets shared state, a request ID, a token-derived account scope, an approved upstream, and a resolved policy on protected routes.

Playback processing order:

1. Account resolution and policy enforcement.
2. Automatic media-version selection.
3. Maximum-quality and direct-play rewriting.
4. Video-transcode fallback.
5. Direct-stream fallback and the Plex decision.

This order prevents optional selectors from restoring versions prohibited by account policy.

Hub processing applies collection visibility first. Optional restriction, styling, watched filtering, interleaving, user-state, and key transforms follow.

## Hub interleaving

`REPLEX_INTERLEAVE=true` merges collection hubs with exactly matching titles. Their children are interleaved so movies and shows from separate libraries can share one row.

Built-in Plex hubs remain separate. Missing keys or inaccessible collections do not fail the home screen.

Transformed hub keys point to `/replex/<style>/...`. Pagination and collection refreshes therefore pass through the same transformations.

## Hub restrictions

`REPLEX_HUB_RESTRICTIONS=true` removes collection hubs not present in the requesting account's visible Plex collection list.

Built-in hubs pass through. Malformed hub metadata fails open for compatibility and produces a warning.

## Watched-item filtering

`REPLEX_EXCLUDE_WATCHED=true` removes watched children from hubs and interleaved collections.

The exact Plex collection label `REPLEX_EXCLUDE_WATCHED` enables filtering for one collection. Filtering changes page sizes, so Replex requests larger source pages where possible.

## Continue Watching and metadata display

- `REPLEX_DISABLE_CONTINUE_WATCHING=true` returns an empty Continue Watching container.
- `REPLEX_DISABLE_RELATED=true` sets `includeRelated=0`.
- `REPLEX_DISABLE_USER_STATE=true` clears visible watched badges without changing Plex history.
- `REPLEX_DISABLE_LEAF_COUNT=true` removes visible episode/leaf counts. It defaults to enabled.

## Shelf and hero styles

A collection becomes hero styled when it has the exact `REPLEXHERO` label. Built-in hubs become heroes when their identifier appears in `REPLEX_HERO_ROWS`.

Plex-native hero hubs, including modern Continue Watching responses, retain hero presentation.

Hero transforms preserve native Plex image entries and add or replace one `coverArt` image. Replex adapts `art`, `thumb`, child type, and metadata for each client family.

Artwork selection order:

1. The item's `REPLEXHEROURL` label value.
2. `/replex/image/hero/<type>/<uuid>`, using Plex provider metadata and the request token.

Generated artwork URLs use forwarded host and protocol values when present. Only trusted reverse proxies should be allowed to supply those headers.

## Automatic media-version selection

`REPLEX_AUTO_SELECT_VERSION=true` selects a source when the client reports a screen resolution but no explicit `mediaIndex`.

The closest pixel density wins. Missing dimensions rank last. Ties prefer the higher-resolution source. Replex writes the original Plex media index.

An explicit client selection remains unchanged unless account policy prohibits it.

## Maximum quality and direct play

`REPLEX_FORCE_MAXIMUM_QUALITY=true` removes bitrate limits, disables automatic quality adjustment, and requests quality 100, direct play, and direct stream.

This does not guarantee direct play. Plex may transcode unsupported codecs, containers, audio, subtitles, or client capabilities.

`REPLEX_DISABLE_TRANSCODE=true` currently activates the same rewrite. It is a preference, not an absolute transcoder prohibition.

`REPLEX_FORCE_DIRECT_PLAY_FOR=4k,1080,720` marks matching selected versions for direct play. It currently runs only when maximum quality or disable transcode also enables that handler.

Global maximum quality conflicts with per-account `max_bitrate`: the global rewrite removes bitrate parameters. Do not combine them.

## Video-transcode fallback

Set `REPLEX_VIDEO_TRANSCODE_FALLBACK_FOR` to a source resolution such as `4k`.

If that source requires video transcoding, Replex selects the highest eligible version with a different resolution. It preserves the original Plex index and requests direct stream.

Policy filters candidates first. Fail-closed identity errors suppress fallback. Audio remuxing alone does not trigger it. Only the first configured resolution is currently used.

## Per-account policy

`REPLEX_RESOLUTION_POLICY_ENABLED=true` enables resolution, bitrate, and collection rules.

Rules match UUID first, then case-sensitive username. Unmatched accounts use `REPLEX_RESOLUTION_DEFAULT`.

Each rule supports:

- `max_resolution`: `480`, `720`, `1080`, `4k`, or `unlimited`;
- `max_bitrate`: a positive kbps cap;
- `visible_collections`: exceptions to globally hidden titles.

```text
REPLEX_RESOLUTION_POLICY_ENABLED=true
REPLEX_USER_RESOLUTION_POLICIES=[{"uuid":"abc-123","max_resolution":"1080","max_bitrate":8000,"visible_collections":["Family 4K"]}]
REPLEX_RESOLUTION_DEFAULT=720
REPLEX_HIDDEN_COLLECTIONS=Family 4K,Adults
REPLEX_RESOLUTION_POLICY_FAIL_CLOSED=true
```

Policy affects:

- metadata, hubs, libraries, and play queues;
- explicit and automatic playback selection;
- requested bitrate;
- direct media-part authorization;
- stream proxy versus redirect behavior;
- collection visibility.

Unknown direct parts are rejected for restricted accounts. Restricted part and transcode-session streams always stay behind Replex.

Classification uses Plex's text and source dimensions. When they disagree, the more restrictive result wins. Unknown resolution is blocked only for restricted users.

### Identity order

1. Plex account identity for the request token.
2. Shared-resource and shared-server matching.
3. `REPLEX_TOKEN_IDENTITY_MAP` by lowercase SHA-256 token fingerprint.
4. Legacy `REPLEX_CLIENT_IDENTITY_MAP`.
5. Client username when `REPLEX_ALLOW_USERNAME_FALLBACK=true`.

The last two values are client-controlled and weaker. Token fingerprint bindings are preferred.

Fail-closed mode rejects protected requests when identity cannot be established. Resolved identities are cached for `REPLEX_IDENTITY_CACHE_TTL` seconds.

## Stream transport

| Request | Redirect setting | Result |
|---|---:|---|
| Resolution or bitrate restricted | either | Proxy through Replex |
| Unrestricted | `false` | Proxy through Replex |
| Unrestricted | `true` | Temporary redirect to stream origin |

`REPLEX_REDIRECT_STREAMS_HOST` changes only the redirect destination. Redirected clients must reach that origin.

Plex may request each byte range through Replex before following the redirect. Redirecting reduces payload bandwidth but not all request traffic.

## Account-isolated caching

Hub, library, artwork, and identity entries are scoped by a one-way token fingerprint. One account cannot prime another account's authenticated data.

- Hub cache: in-memory raw Plex payloads, transformed per request.
- Library cache: on-disk raw payloads, re-filtered against current policy.
- Artwork cache: memory and disk, preserving content type.
- Identity cache: resolved identity by full token fingerprint.
- Part cache: media facts only; authorization is re-evaluated each request.
- Plex Web cache: immutable static assets in memory.

Old hubs are served immediately after `REPLEX_HUB_STALE_TTL`. One rate-limited refresh runs in the background.

Playback stop/scrobble requests and Plex webhooks mark hub ages stale without deleting current payloads.

The warmer runs every `REPLEX_WARM_INTERVAL` seconds. It prefetches hubs, poster thumbnails, and up to 250 library items per section for each configured token.

`0` prevents the warmer from starting. A non-empty `REPLEX_WARM_TOKENS` list replaces, rather than extends, the owner-token default.

## Plex Web asset caching

Files under `/web/*` come from Plex. Content-hashed assets receive a one-year immutable policy.

`index.html` and translation bundles remain `no-cache` so updates can propagate.

## Webhook invalidation

Configure Plex to POST to `/replex/webhooks`. Events beginning with `media.` mark hubs stale.

Plex webhooks may require Plex Pass and a public HTTPS endpoint.

## Notification/watchlist bootstrap

`REPLEX_NTF_WATCHLIST_FORCE=true` performs remote account writes when `/media/providers` is requested.

The job resolves the user, enables Plex's new-library notification, and opts out of Plex VOD and music providers.

Only one job runs per token-derived account scope. Further attempts cool down for one hour. Leave this disabled unless that exact account change is wanted.

## Dynamic multi-server routing

Replex can decode a base32 origin from `<encoded>.replex.stream`.

The decoded URL must exactly match `REPLEX_HOST`, `REPLEX_REDIRECT_STREAMS_HOST`, or `REPLEX_ALLOWED_UPSTREAM_HOSTS`. Others are rejected before tokens are forwarded.

## Compatibility bypasses

Plexamp and Live TV skip optional presentation transforms that may interfere with those clients.

They do not bypass resolution policy, direct-part checks, or collection visibility.
