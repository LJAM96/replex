# Operations, security, and diagnostics

## Deployment model

Replex should be the Plex URL visible to clients. Set the public URL in Plex's Custom server access URLs and disable GDM if it advertises the origin.

For enforcement, network controls must stop restricted clients reaching Plex directly. Replex cannot limit a valid Plex token used against the origin.

## Reverse proxy requirements

- Forward the original host and protocol for hero artwork URLs.
- Preserve Plex token, client, range, and playback-session headers.
- Allow long-lived and range stream responses.
- Keep transcode-session and media-part routes behind Replex.
- Accept forwarded headers only from a trusted proxy.
- Use HTTPS because several Plex clients reject insecure custom URLs.

Do not bypass `/video/:/transcode/universal/session/*` or `/library/parts/*` when policies are enabled.

Built-in ACME TLS uses `REPLEX_SSL_ENABLE` and `REPLEX_SSL_DOMAIN`. A reverse proxy can instead terminate TLS on the private Replex listener.

## Authentication for admin endpoints

Admin endpoints require the configured `REPLEX_TOKEN`. Supply it in `X-Plex-Token`, as Plex clients do.

Avoid embedding owner tokens in shared command history. Prefer a protected environment variable or secret file.

## Health endpoints

`GET /health/live` is public and checks process liveness only.

`GET /replex/admin/health/ready` requests `/:/identity` from Plex. It returns `200` when Plex is healthy and `503` otherwise.

Readiness also reports cache counts, disk bytes, background jobs, policy generation, and fail-closed state.

```sh
curl -H "X-Plex-Token: $REPLEX_ADMIN_TOKEN" \
  https://plex.example.com/replex/admin/health/ready
```

## Metrics

`GET /replex/admin/metrics` returns Prometheus text.

| Metric | Meaning |
|---|---|
| `replex_policy_rejects_total` | Requests rejected by policy or part checks. |
| `replex_cache_hits_total` | Instrumented hub, library, and artwork hits. |
| `replex_cache_misses_total` | Instrumented hub, library, and artwork misses. |
| `replex_redirects_total` | Stream redirects issued. |
| `replex_upstream_failures_total` | Failed proxy and readiness requests. |
| `replex_upstream_requests_total` | Shared upstream proxy requests. |
| `replex_upstream_latency_milliseconds_total` | Cumulative latency for those requests. |
| `replex_task_failures_total` | Failed supervised jobs. |

Divide cumulative latency by request count for the process-lifetime mean.

If a scraper cannot send `X-Plex-Token`, let a trusted proxy translate a protected credential into that header.

## Cache inspection and purge

`GET /replex/admin/cache` reports memory entry counts and on-disk bytes.

`DELETE /replex/admin/cache/<class>` supports:

| Class | Default scope | With `?account=all` |
|---|---|---|
| `metadata` | Calling token's scoped keys | All metadata/hubs |
| `identity` | Calling token fingerprint | All identities |
| `photos` | Calling token's artwork | All artwork memory |
| `parts` | Rejected | All part facts |
| `global` | Rejected | All global/web assets |
| `disk` | Rejected | All files in the cache directory |

```sh
curl -X DELETE -H "X-Plex-Token: $REPLEX_ADMIN_TOKEN" \
  https://plex.example.com/replex/admin/cache/photos

curl -X DELETE -H "X-Plex-Token: $REPLEX_ADMIN_TOKEN" \
  'https://plex.example.com/replex/admin/cache/disk?account=all'
```

Disk purge removes files only below `REPLEX_DISK_CACHE_DIR`. Cache data is disposable and refetched on demand.

## Policy reload

`POST /replex/admin/policy/reload` reparses the current process environment and atomically replaces the runtime policy snapshot.

Rules, default limit, hidden collections, and fail-closed behavior reload. Existing requests keep their snapshot; later requests see the new generation.

Changing `REPLEX_RESOLUTION_POLICY_ENABLED` returns `409`. Enabling or disabling policy changes the route graph and requires restart.

Other non-policy settings are not replaced by this endpoint.

An external container environment normally cannot change after startup. Update deployment configuration and restart if the supervisor cannot mutate process variables.

## Playback explanation

`GET /replex/admin/playback/explain?path=/library/metadata/<id>` authenticates with the owner token and fetches that item.

It reports policy generation, original media indexes, source IDs, and dimensions.

Policy outcomes appear only when a request security context exists. The admin route normally reports policy source as unavailable, so allow/reject and selected-index fields may be empty.

The endpoint does not start playback or change Plex state.

## Webhooks

Configure Plex to POST events to:

```text
https://plex.example.com/replex/webhooks
```

`media.*` events mark hubs stale. Current payloads remain available while the next request starts a single-flight refresh.

The webhook is not authenticated by Replex. Apply network or reverse-proxy controls if unsolicited access is a concern.

## Logging

```text
RUST_LOG=replex=debug,salvo=info
```

Logs contain paths but not query strings. Plex tokens may be query parameters, so full URI logging would disclose credentials.

Accounts appear as short SHA-256 fingerprint scopes. Useful events include policy decisions, stream transport, cache refresh, warming, upstream errors, and task results.

## CORS

Browser access defaults to `https://app.plex.tv` and `https://plex.tv`. Override it with comma-separated `REPLEX_CORS_ALLOWED_ORIGINS`.

CORS is a browser policy, not authentication. Non-browser clients still require network and Plex-token controls.

## Dynamic upstream security

Encoded `*.replex.stream` hosts work only when the decoded URL exactly matches an allowed origin.

Add legitimate servers to `REPLEX_ALLOWED_UPSTREAM_HOSTS`. Do not use wildcard behavior in a reverse proxy to weaken this boundary.

## Troubleshooting

### Plex Web works but native clients bypass Replex

- Disable Plex GDM/local discovery.
- Advertise only the public Replex custom URL.
- Clear the client's server cache.
- Firewall direct Plex access.

### Hero rows lack artwork

- Check `REPLEXHERO` or `REPLEX_HERO_ROWS`.
- Configure `REPLEX_TOKEN` and preserve request tokens.
- Forward the public host and protocol.
- Check client-specific rendering limits.

### Restricted playback is unexpectedly allowed

- Confirm the client cannot reach Plex directly.
- Confirm policy was enabled at startup.
- Use the playback explanation endpoint.
- Check identity source and policy logs.
- Keep streams behind Replex and redirects disabled.

### A restricted direct part returns 403

Unknown parts are blocked. Load item metadata or make a policy-aware playback decision so Replex can cache source facts.

If the classified source exceeds policy, the rejection is intentional.

### Home rows are stale

- Check stale TTL and warmer interval.
- Ensure playback events pass through Replex.
- Configure Plex webhooks for outside changes.
- Inspect readiness, task metrics, and warmer logs.

### Redirected playback fails

- Ensure the client reaches `REPLEX_REDIRECT_STREAMS_HOST`.
- Confirm the origin accepts the same paths and token.
- Disable redirects for clients with broken range handling.

## Known client limitations

- Android hero hubs may not paginate and can show a bounded first page.
- Android mobile may crop hero artwork differently.
- Plex client behavior can change independently after updates.
- Redirect mode still sends initial range requests through Replex.
