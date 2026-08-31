# Configuration reference

Replex reads `REPLEX_*` environment variables. Lists are comma-separated unless documented as JSON.

## Server and network

| Setting | Default | Function |
|---|---|---|
| `REPLEX_HOST` | required | Absolute HTTP(S) Plex base URL, usually `http://plex:32400`. |
| `REPLEX_TOKEN` | empty | Owner token for identity fallback, hero art, warming, and admin APIs. |
| `REPLEX_PORT` | `80`, or `443` with TLS | Listen port from 1 to 65535. |
| `REPLEX_SSL_ENABLE` | `false` | Enable built-in ACME TLS. |
| `REPLEX_SSL_DOMAIN` | empty | ACME domain, required with built-in TLS. Certificates use `/data/acme/letsencrypt`. |
| `REPLEX_CORS_ALLOWED_ORIGINS` | Plex web origins | Comma-separated browser origins. Read directly from the environment. |
| `REPLEX_REDIRECT_STREAMS` | `false` | Redirect unrestricted stream payloads. Restricted streams always proxy. |
| `REPLEX_REDIRECT_STREAMS_HOST` | `REPLEX_HOST` | Alternate stream redirect base URL. |
| `REPLEX_ALLOWED_UPSTREAM_HOSTS` | empty | Extra exact base URLs allowed for encoded multi-server routing. |

Default CORS origins are `https://app.plex.tv` and `https://plex.tv`. Credentials are allowed, but methods and headers are restricted to the Plex API set.

## Hub and presentation

| Setting | Default | Function |
|---|---:|---|
| `REPLEX_INTERLEAVE` | `true` | Merge same-titled collection hubs and interleave children. |
| `REPLEX_HUB_RESTRICTIONS` | `true` | Remove collection hubs unavailable to the account. |
| `REPLEX_EXCLUDE_WATCHED` | `true` | Remove watched hub and collection children. |
| `REPLEX_HERO_ROWS` | empty | Comma-separated Plex hub identifiers to hero-style. |
| `REPLEX_DISABLE_CONTINUE_WATCHING` | `false` | Return an empty Continue Watching container. |
| `REPLEX_DISABLE_USER_STATE` | `false` | Clear visible watched badges without changing history. |
| `REPLEX_DISABLE_LEAF_COUNT` | `true` | Remove visible episode/leaf counts. |
| `REPLEX_DISABLE_RELATED` | `false` | Set `includeRelated=0`. |

The `REPLEXHERO` collection label enables hero style. `REPLEXHEROURL` supplies a custom hero URL. `REPLEX_EXCLUDE_WATCHED` enables watched filtering per collection.

## Playback and selection

| Setting | Default | Function |
|---|---:|---|
| `REPLEX_AUTO_SELECT_VERSION` | `false` | Select the source closest to client screen resolution. |
| `REPLEX_FORCE_MAXIMUM_QUALITY` | `false` | Remove bitrate limits and request quality 100/direct play/direct stream. |
| `REPLEX_DISABLE_TRANSCODE` | `false` | Currently performs the maximum-quality rewrite; not an absolute block. |
| `REPLEX_FORCE_DIRECT_PLAY_FOR` | empty | Resolution labels to request direct play; active only with maximum quality or disable transcode. |
| `REPLEX_VIDEO_TRANSCODE_FALLBACK_FOR` | empty | Source resolution such as `4k` that may fall back when video transcodes. |

Only the first transcode-fallback value is currently used. Forced direct play can fail on unsupported clients.

## Account policy

| Setting | Default | Function |
|---|---:|---|
| `REPLEX_RESOLUTION_POLICY_ENABLED` | `false` | Register policy-aware metadata, playback, part, and stream routes. |
| `REPLEX_USER_RESOLUTION_POLICIES` | `[]` | JSON array of account rules. |
| `REPLEX_RESOLUTION_DEFAULT` | `unlimited` | Limit for accounts without a matching rule. |
| `REPLEX_RESOLUTION_POLICY_FAIL_CLOSED` | `true` | Reject protected requests when identity is unavailable. |
| `REPLEX_HIDDEN_COLLECTIONS` | empty | Exact, case-sensitive titles hidden by default. |

Resolution values are `480`, `720`, `1080`/`2k`, `4k`/`2160`, and `unlimited`.

Policy fields:

| Field | Required | Meaning |
|---|---:|---|
| `uuid` | one identity | Stable Plex account UUID; preferred. |
| `username` | one identity | Case-sensitive Plex username. |
| `max_resolution` | yes | Account resolution limit. |
| `max_bitrate` | no | Positive bitrate cap in kbps. |
| `visible_collections` | no | Exact title exceptions to hidden collections. |

```json
[
  {
    "uuid": "account-uuid",
    "username": "jodie",
    "max_resolution": "1080",
    "max_bitrate": 8000,
    "visible_collections": ["Family 4K"]
  }
]
```

Put compact JSON on one line in an environment variable. Quote it according to shell or Compose syntax.

## Identity compatibility

| Setting | Default | Function |
|---|---:|---|
| `REPLEX_IDENTITY_CACHE_TTL` | `3600` | Seconds to cache resolved identities. |
| `REPLEX_IDENTITY_API_BASE` | `https://plex.tv` | Compatible identity API base, mainly for testing. |
| `REPLEX_TOKEN_IDENTITY_MAP` | `{}` | JSON map from lowercase SHA-256 token fingerprint to identity binding. |
| `REPLEX_CLIENT_IDENTITY_MAP` | `{}` | Legacy client-identifier-to-username JSON map. |
| `REPLEX_ALLOW_USERNAME_FALLBACK` | `false` | Trust client username after stronger methods fail. |

Compact token binding:

```json
{"<sha256-token>":"jodie"}
```

Binding with a client identifier constraint:

```json
{"<sha256-token>":{"username":"jodie","client_identifier":"living-room-tv"}}
```

Generate a lowercase fingerprint:

```sh
printf '%s' 'YOUR_TOKEN' | shasum -a 256
```

Avoid leaving the raw token in shared command history.

## Cache and background work

| Setting | Default | Function |
|---|---:|---|
| `REPLEX_CACHE_TTL` | `1800` | General cache TTL in seconds. `0` expires immediately. |
| `REPLEX_HUB_STALE_TTL` | `300` | Age for stale-serving and background hub refresh. `0` disables checks. |
| `REPLEX_WARM_INTERVAL` | `300` | Seconds between warming cycles. `0` prevents task startup. |
| `REPLEX_WARM_TOKENS` | empty | Tokens to warm instead of the owner-token default. |
| `REPLEX_DISK_CACHE_DIR` | `/data/replex-cache` or `./replex-cache` | Disk cache directory. Read directly from the environment. |
| `REPLEX_DISK_CACHE_MAX_GB` | `45` | Positive GiB high-water mark. Cleanup targets 85%. |

`/data/replex-cache` is used when `/data` exists. Otherwise Replex uses `./replex-cache`.

Each warm token has an isolated namespace. A non-empty `REPLEX_WARM_TOKENS` replaces the owner token rather than adding to it.

## Notification behavior

| Setting | Default | Function |
|---|---:|---|
| `REPLEX_NTF_WATCHLIST_FORCE` | `false` | Enable a Plex notification and opt out of Plex VOD/music providers. |

This performs remote account writes on `/media/providers`, once per scope per one-hour cooldown. Leave it disabled unless wanted.

## Logging and diagnostics

| Setting | Default | Function |
|---|---:|---|
| `RUST_LOG` | `info` | Tracing filter, for example `replex=debug,salvo=info`. |
| `REPLEX_ENABLE_CONSOLE` | `false` | Start Tokio console instrumentation. |

Request logs exclude query strings because Plex tokens can appear there. They include paths, request IDs, fingerprint scopes, and policy context.

## Accepted legacy or reserved settings

These settings are parsed but do not currently affect active requests:

| Setting | Status |
|---|---|
| `REPLEX_CACHE_ROWS` | Legacy; no current runtime effect. |
| `REPLEX_CACHE_ROWS_REFRESH` | Deprecated; no current runtime effect. |
| `REPLEX_NEWRELIC_API_KEY` | Reserved; the exporter is disabled. |
| `REPLEX_TEST_SCRIPT` | Reserved; Rhai is not registered in the active pipeline. |

This distinction prevents an accepted variable from being mistaken for a working feature.
