# Replex

Replex is a policy-aware reverse proxy for Plex Media Server. It sits between Plex clients and Plex, transforms selected requests and responses, and proxies everything else unchanged.

![Replex hero-style home screen](./examplewithhero.png)

## What Replex provides

- Interleaved rows combining same-named collection hubs across libraries.
- Shelf and client-aware hero styles, including custom hero artwork.
- Watched-item, badge, episode-count, related-content, and Continue Watching controls.
- Version selection, maximum-quality rewriting, direct-play preferences, and transcode fallback.
- Per-account resolution, bitrate, and collection-visibility policies.
- Account-isolated caches, stale-while-revalidate, and background warming.
- Safe stream proxy/redirect controls for restricted and unrestricted accounts.
- Readiness, metrics, cache administration, policy reload, and playback diagnostics.
- Plex Web asset caching, webhooks, restricted CORS, TLS, and allowlisted multi-server routing.

See [Feature behavior](docs/FEATURES.md) for every feature and the order in which transformations run.

## How it works

```text
Plex client
    |
    v
Replex request context and account policy
    |
    +-- metadata/hubs --> account cache --> transforms --> client
    +-- playback ------> policy and version selection --> Plex
    +-- streams -------> proxy, or redirect when unrestricted
    `-- other paths ---> transparent Plex proxy
```

Plex tokens are forwarded only to configured upstreams. Logs and cache keys use one-way token fingerprints instead of raw tokens.

## Quick start with Docker Compose

```yaml
services:
  plex:
    image: lscr.io/linuxserver/plex:latest
    container_name: plex
    environment:
      PUID: 1000
      PGID: 1000
      TZ: Etc/UTC
      VERSION: docker
      PLEX_CLAIM: ""
    volumes:
      - /path/to/plex-config:/config
      - /path/to/tv:/tv
      - /path/to/movies:/movies
    restart: unless-stopped

  replex:
    image: ghcr.io/sarendsen/replex:latest
    container_name: replex
    environment:
      REPLEX_HOST: http://plex:32400
      REPLEX_TOKEN: "your-server-owner-token"
      RUST_LOG: info
    ports:
      - "3001:80"
    volumes:
      - replex-cache:/data/replex-cache
    depends_on:
      - plex
    restart: unless-stopped

volumes:
  replex-cache:
```

Then:

1. Set Plex's **Settings → Network → Custom server access URLs** to the public Replex URL.
2. Disable Plex GDM if clients discover the Plex origin directly.
3. Clear old server connections from clients or clear their cache.
4. Block restricted clients from reaching Plex directly when using policy enforcement.

The maintained example is [docker/compose.example.yml](docker/compose.example.yml).

## Essential configuration

Only `REPLEX_HOST` is required. `REPLEX_TOKEN` is strongly recommended for owner identity, hero art, warming, and authenticated administration.

```text
REPLEX_HOST=http://plex:32400
REPLEX_TOKEN=<server-owner Plex token>
REPLEX_PORT=80
```

Configuration uses `REPLEX_*` environment variables. Invalid URLs, ports, policy identities, bitrate limits, and disk-cache sizes fail validation at startup.

See [Configuration reference](docs/CONFIGURATION.md) for every setting, default, format, interaction, and runtime status.

## Security boundary

Policies apply only to traffic passing through Replex. A client that reaches Plex directly can bypass metadata filtering, playback rewriting, and direct-part checks.

For enforcement:

- firewall Plex so restricted clients cannot reach it directly;
- keep stream and part routes behind Replex;
- keep `REPLEX_REDIRECT_STREAMS=false` for hardened deployments;
- leave `REPLEX_RESOLUTION_POLICY_FAIL_CLOSED=true`;
- prefer token-fingerprint identity bindings over client-controlled fallbacks.

See [Operations](docs/OPERATIONS.md) for deployment, endpoint, metrics, cache, and troubleshooting guidance.

## Documentation

- [Feature behavior](docs/FEATURES.md)
- [Configuration reference](docs/CONFIGURATION.md)
- [Operations, security, and diagnostics](docs/OPERATIONS.md)
- [Example Compose deployment](docker/compose.example.yml)
- [Example Rhai script](examples/reorder_media.rhai) — retained as an example; scripting is not active in the current pipeline.

## Compatibility

Hero metadata adapts to Plex Web, iOS, tvOS, Roku, Android, and Android TV. Plexamp and Live TV skip optional presentation rewrites, but security and collection visibility stay active.

Plex changes private APIs and rendering behavior over time. Validate important clients after Plex server or client upgrades.

## Project status

The original maintainer no longer uses Plex. The project welcomes active maintainers and contributors.
