# Replex Security, Reliability and Architecture Implementation Plan

## Purpose

This document defines the recommended implementation sequence following the independent review of the current Replex working tree. It is intended to be implementation ready and should be treated as the engineering roadmap for the next hardening cycle.

The current uncommitted hardening changes in `src/cache.rs`, `src/models.rs`, `src/plex_client.rs`, `src/routes.rs`, and `src/utils.rs` should be preserved as the baseline.

The primary goal is to make per account resolution enforcement trustworthy across every route and cache path, then improve reliability, performance, maintainability, testing, and deployment clarity.

## Desired end state

Replex should have one consistent identity and policy decision for every request that can expose protected Plex metadata or media bytes.

Security decisions must occur before compatibility shortcuts, presentation transforms, cache hits, redirect choices, or client specific behaviour.

Caches should store data, not authorisation decisions. A cache entry created by one account must never become proof that another account is entitled to receive that data.

Plex remains the source of truth for account access unless Replex can independently prove the same authorisation decision from trusted state.

Restricted users must not be able to bypass limits by changing client headers, selecting another route, requesting a direct part, manipulating playback parameters, or hitting data cached by another account.

## Core design principles

### Security before compatibility

Any middleware that skips normal Replex behaviour must sit after mandatory identity and policy enforcement. Client supplied values such as `X-Plex-Product`, `X-Plex-Username`, `X-Plex-Client-Identifier`, screen resolution, path fields, and playback query parameters are untrusted input unless explicitly bound to trusted state.

### Caches store facts, not permissions

Account scoped Plex metadata should be account scoped in cache by default. Shared caching should be limited to genuinely public/static resources or resources that are independently authorised before a shared entry is served.

### Resolve policy once

Identity and effective resolution policy should be resolved once per protected request and reused by all downstream handlers and transforms.

### Validate configuration once

Configuration should be parsed and validated at startup, then held in application state rather than repeatedly reconstructed with `Config::figment().extract()` on hot paths.

### Reuse network clients

Reqwest clients should be long lived. Reusing them enables connection pooling, keep alive, TLS session reuse, and consistent timeout behaviour.

## Phase 1: Remove the Plexamp and Live TV security bypass

**Implementation status: complete.** The global Plexamp and Live TV shortcut
now records request-scoped compatibility state instead of proxying early or
calling `skip_rest()`. Mandatory resolution, direct-part and account collection
visibility enforcement ignore that state. Optional playback, hub styling,
interleaving, request-shaping, notification, related-item and photo-cache
behaviour can bypass themselves for compatibility. Regression coverage now
verifies both Plexamp and Live TV classification cannot bypass the restricted
direct-part guard, Plexamp cannot bypass playback media-version enforcement,
and Plexamp hub requests still remove collections hidden by account policy.

### Problem

`should_skip` currently executes globally before protected routes. It trusts client controlled product and path values, directly proxies Plexamp or Live TV requests, then calls `skip_rest()`.

This can bypass `protected_redirect_stream`, `enforce_resolution_policy`, and metadata filtering.

### Implementation

Refactor `should_skip` into a classifier only. It should set request scoped state such as `skip_optional_transforms = true` and then return normally.

Mandatory security handlers must ignore this compatibility flag.

Only optional presentation logic such as hub styling, interleaving, watched filtering, cosmetic transforms, and client specific shaping should inspect the flag and skip themselves.

Never let `should_skip` call the upstream proxy or `skip_rest()` before security enforcement.

### Target files

`src/routes.rs`

Potentially `src/models.rs` or a new request context module.

### Tests

Test a restricted 1080p user making a prohibited 4K request with a normal product value and with `X-Plex-Product: Plexamp`. Both must receive the same enforcement decision.

Repeat for direct part access.

Test Live TV classification to confirm optional transforms can still be bypassed without bypassing mandatory protection on protected routes.

### Acceptance criteria

No client supplied product or path value can skip identity or policy enforcement.

## Phase 2: Secure identity fallback handling

**Implementation status: complete.** Opaque Plex tokens can now be bound to an
administrator configured username using the lowercase SHA256 fingerprint of the
token through `REPLEX_TOKEN_IDENTITY_MAP`. Bindings optionally support an
additional `client_identifier` constraint, but the token fingerprint remains
the credential. Verified Plex identity, shared-resource identity, and admin
shared-server token matching all take precedence. A constraint mismatch is
fail-closed and cannot fall through to weaker fallbacks. The old
`client_identity_map` remains available only as a documented legacy migration
fallback, and username-header fallback remains disabled unless explicitly
enabled. Regression coverage verifies fingerprint resolution, same-client-ID
token isolation, verified-identity precedence, optional client constraints,
and explicit opt-in for weak fallbacks.

### Problem

`client_identity_map` is safer than username fallback but still uses a client supplied identifier as the binding. A caller can choose `X-Plex-Client-Identifier` freely.

### Recommended design

Introduce administrator configured token fingerprint bindings using SHA256 of the Plex token.

Do not store raw Plex tokens in logs, cache keys, or diagnostics.

Conceptually:

```json
{
  "token_sha256_fingerprint": "username"
}
```

A client identifier may optionally be an additional constraint, but should not be the primary credential.

Preferred identity order:

Verified Plex identity

Shared user resolution through Plex resources

Shared server token matching

Administrator configured token fingerprint binding

Optional legacy client identifier mapping

Optional unverified username fallback

Failure

### Compatibility

Keep `client_identity_map` temporarily for migration but label it legacy compatibility.

Keep username fallback disabled by default and last in the chain.

### Target files

`src/config.rs`

`src/plex_client.rs`

`src/resolution_policy.rs`

`tests/identity.rs`

`README.md`

### Tests

Verify a known opaque token fingerprint resolves correctly.

Verify another token using the same client identifier cannot inherit that identity.

Verify verified Plex identity overrides all fallbacks.

Verify weak fallback modes require explicit opt in.

## Phase 3: Account scope all authenticated hub metadata

**Implementation status: complete.** Every authenticated hub cache key now
includes a hash-derived account scope, including `/hubs/sections/<id>`.
The warmer uses the same `hub_cache_key` path as live requests. Regression
coverage verifies different tokens produce different section-hub keys and an
end-to-end route test proves two accounts requesting the same section cannot
receive each other's cached upstream payload.

### Problem

`/hubs/sections/<id>` is currently treated as globally reusable even though library access is account specific.

A section hub cached using an authorised account can potentially be served to an account Plex would not authorise for that section.

### Implementation

Make all authenticated hub cache keys include the requesting token fingerprint.

This should include section hubs, Continue Watching, On Deck, promoted hubs, home hubs, and future authenticated hub routes.

Conceptual key:

```text
hubcache:u:<token scope>:<canonical path>
```

Do not use an anonymous global fallback for authenticated Plex library metadata.

The warmer should continue supporting multiple tokens and should warm each account independently using the same cache key function as live requests.

### Target files

`src/hub_cache.rs`

`src/routes.rs`

`src/test_helpers/mod.rs`

### Tests

Account A primes a section it is allowed to access.

Account B requests the same section but is not shared that library.

Account B must not receive A's cached payload.

Verify two accounts generate different keys for the same hub path.

Verify warmer and live requests produce identical keys for the same token.

## Phase 4: Account scope Plex artwork caching

**Implementation status: complete.** Plex artwork keys now combine the
canonical image request with a hash-derived account scope. Top-level and nested
Plex token query parameters remain canonicalised out of the image identity, but
the requesting account scope is retained separately. The background warmer
uses the same key builder, memory and disk entries share that scoped key, and
public `/web/` assets remain globally cacheable. Tests verify cross-account
artwork memory-cache isolation and continued `/web/` cache sharing.

### Problem

Photo cache hits are returned before Plex is contacted and the current canonical key strips token identity. A cached image can therefore bypass normal Plex permission checks.

### Implementation

Scope Plex library artwork cache entries by token fingerprint.

Conceptual key:

```text
photo:u:<token scope>:<canonical image request>
```

Continue canonicalising cosmetic image parameters where safe, but retain account scope.

Keep `/web/` static application assets globally cacheable because they are public application resources rather than protected library data.

The warmer must populate photo entries in the same account namespace as the token used for that warm cycle.

### Target files

`src/routes.rs`

`src/hub_cache.rs`

Potential future `src/cache/images.rs`.

### Tests

Account A caches an image.

Account B requests the same image path.

Confirm B cannot hit A's memory or disk cache entry.

Verify `/web/` asset sharing still works.

## Phase 5: Correct stream redirect semantics

**Implementation status: complete.** Stream transport is now selected by one
`stream_delivery` decision: resolution or bitrate restricted accounts always
proxy, fully unrestricted accounts redirect only when
`REPLEX_REDIRECT_STREAMS=true`, and fail-open
identity handling preserves the configured transport mode. Direct-part tests
exercise restricted proxying, unrestricted redirect/proxy behaviour, and the
fail-open redirect/proxy matrix; a pure matrix regression test covers all four
restricted/unrestricted and redirect enabled/disabled combinations.

### Required behaviour

Resolution or bitrate restricted account means always proxy through Replex.

Fully unrestricted account plus `REPLEX_REDIRECT_STREAMS=true` means redirect.

Fully unrestricted account plus `REPLEX_REDIRECT_STREAMS=false` means proxy.

Identity fail open should remove the restriction decision but must not silently change the configured transport mode.

### Implementation

Create one helper that decides stream delivery after the security decision.

Conceptually:

```text
restricted -> proxy
unrestricted + redirects enabled -> redirect
unrestricted + redirects disabled -> proxy
```

Use it from all stream paths rather than reproducing transport decisions in several branches.

### Target files

`src/routes.rs`

`README.md`

`docker/portainer-stack.yml`

`tests/direct_parts.rs`

`tests/playback.rs`

### Tests

Cover the full restricted/unrestricted and redirect true/false matrix.

Also test identity fail open behaviour.

## Phase 6: Remove dead `REPLEX_STRICT_STREAM_GUARD` configuration

**Implementation status: complete.** `REPLEX_STRICT_STREAM_GUARD` has been
removed from runtime configuration, examples, and tests. Unknown parts are now
unconditionally blocked for restricted accounts. There is no compatibility
switch capable of weakening this boundary.

The runtime no longer reads this setting while restricted users always block unknown parts.

The preferred design is to remove the setting and make strict unknown part blocking mandatory for restricted users.

If compatibility requires a transition period, retain it for one release as deprecated and ignored with a startup warning.

Update configuration examples and README documentation at the same time.

## Phase 7: Replace cached part permission booleans with media classification facts

**Implementation status: complete.** The account and policy scoped boolean
`PART_POLICY_CACHE` has been replaced with `PART_MEDIA_CACHE`, keyed only by
Plex part ID and containing immutable resolution and source bitrate
classification facts. Metadata and playback paths populate those facts before
policy filtering, while direct part requests evaluate the current account
policy at request time. Regression coverage verifies the same cached 4K
classification is denied under a 1080p policy and immediately permitted after
the policy changes to 4K without re-populating the cache. Bitrate-only policies
also evaluate the cached source bitrate so a direct original part cannot bypass
the configured bitrate cap.

### Original design

`PART_POLICY_CACHE` stores a per user boolean authorisation result. The current key is much safer than the old part only key, but a cleaner model is available.

### Recommended design

Cache immutable part classification data instead of an authorisation result.

Conceptually:

```text
part ID -> classified media information
```

When a restricted user requests the part, evaluate the current request policy against that cached media classification.

This separates facts from permissions.

Policy changes then take effect immediately without policy fingerprints in the cache key.

### Target files

`src/plex_client.rs`

`src/resolution_policy.rs`

`src/routes.rs`

`tests/direct_parts.rs`

## Phase 8: Harden the disk cache

### Cleanup threshold

Replace the fixed 5 GiB subtraction with a proportional cleanup target, for example 85 percent of configured maximum.

This avoids wiping the entire cache when the maximum is configured below 5 GiB.

### Record decoding

Treat every stored length as untrusted.

Use checked arithmetic for all offsets.

Before reading the body length, verify the content type length still leaves at least eight bytes in the buffer.

Verify body length conversion to `usize` is safe.

Verify body start plus body length cannot overflow and remains inside the file.

Invalid records should return `None`, be evicted, and be refetched.

### Concurrent writes

Replace deterministic `<hash>.tmp` files with unique temporary names in the same directory.

Possible inputs include PID plus an atomic counter or random value.

Only update size accounting after the rename succeeds.

Do not subtract the old entry before the replacement is safely in place.

### Tests

Test 1 GiB and sub 5 GiB cache limits.

Test corrupt content type length.

Test truncated body length.

Test body length beyond file size.

Test concurrent writes to one key.

Test stale temporary files.

Test successful and failed overwrites with larger and smaller data.

### Target file

`src/disk_cache.rs`

## Phase 9: Introduce shared application state

### Goal

Stop repeatedly parsing configuration and constructing Reqwest clients in request paths.

### Proposed structure

```rust
pub struct AppState {
    pub config: Arc<Config>,
    pub plex_http: reqwest_middleware::ClientWithMiddleware,
    pub proxy_http: reqwest::Client,
    pub identity_http: reqwest::Client,
}
```

The exact split may be simplified if one client configuration can satisfy all use cases.

Store this in Salvo application state or an equivalent shared mechanism.

### Startup validation

Validate Plex host URL, redirect host, identity API base, policy configuration, cache limits, token fingerprint mappings, and incompatible fallback combinations once at startup.

Return clear startup errors rather than allowing malformed configuration to fail later inside request handlers.

### HTTP clients

Create clients once.

Reuse connection pools and keep alive.

Define timeouts centrally.

Do not create a new Reqwest client inside `default_proxy`, `PlexClient::from_context`, hub warmer functions, or web asset fetching.

### PlexClient direction

Make `PlexClient` a lightweight request wrapper containing request account headers and references to shared services rather than its own new network client.

### Target files

`src/main.rs`

`src/lib.rs`

`src/config.rs`

`src/plex_client.rs`

`src/utils.rs`

`src/web_assets.rs`

`src/hub_cache.rs`

Potential new `src/state.rs`.

## Phase 10: Add a request scoped security context

### Proposed structure

```rust
pub struct RequestSecurityContext {
    pub identity: UserIdentity,
    pub token_scope: String,
    pub policy: ResolutionPolicy,
    pub identity_source: IdentitySource,
}
```

`IdentitySource` should distinguish verified Plex identity, shared resource resolution, token fingerprint binding, legacy client mapping, and username fallback.

Resolve this once for protected requests and store it in `Depot`.

Handlers and transforms should consume the stored context instead of independently calling identity and policy resolution.

### Benefits

One fail open/fail closed decision.

Less repeated identity work.

Simpler logging.

Simpler tests.

Fewer opportunities for different routes to use different trust rules.

## Phase 11: Refactor the transform pipeline

### Borrow metadata in filters

Change `filter_metadata` to borrow `&MetaData` instead of taking owned `MetaData` where possible.

This removes large clones for every transform and item.

### Remove optional key unwrap assumptions

Do not identify filtered children by `child.key.clone().unwrap()`.

Filter by index or construct a new result vector so metadata without a key can be processed safely.

### Implement or remove container filtering

`filter_mediacontainer` exists but is not used by the active pipeline. Either integrate it deliberately with tests or remove it.

### Nested policy filtering

Recursively remove playable nested items whose media exists but contains no permitted version.

Preserve structural nodes such as shows and seasons when they still contain permitted descendants.

Remove empty hubs where appropriate.

### Policy lookup optimisation

Pass the request security context into transforms so policy lookup is not repeated for each metadata item.

### Target files

`src/transform/mod.rs`

`src/transform/resolution_policy.rs`

Other transform implementations as required.

## Phase 12: Split oversized modules after security work is stable

Do not combine major file movement with the first security fixes.

After security behaviour is tested, split by responsibility.

Suggested routes layout:

```text
src/routes/mod.rs
src/routes/playback.rs
src/routes/streams.rs
src/routes/hubs.rs
src/routes/library.rs
src/routes/images.rs
src/routes/webhooks.rs
src/routes/web.rs
```

Suggested authentication layout:

```text
src/auth/mod.rs
src/auth/identity.rs
src/auth/context.rs
src/auth/policy.rs
```

Suggested cache layout:

```text
src/cache/mod.rs
src/cache/memory.rs
src/cache/disk.rs
src/cache/hubs.rs
src/cache/library.rs
src/cache/images.rs
```

Keep `PlexClient` thin and move specialised account and metadata API helpers into focused modules.

## Phase 13: Complete panic removal

Audit every non test `unwrap()` and `expect()` reachable from a request.

Classify each as a proven internal invariant, external data that must return an error, or legacy code to replace.

Network helpers such as collection, hub, and item fetches should propagate errors rather than panic.

Malformed Plex responses should normally become `502 Bad Gateway` or a typed upstream error.

Malformed required client input should normally become `400 Bad Request`. Malformed optional feature input should disable that optional behaviour rather than crash the request.

Introduce a small application error enum covering authentication, forbidden policy decisions, invalid client input, upstream transport failures, upstream parse failures, and configuration errors.

Map these consistently to HTTP status codes.

## Phase 14: Expand adversarial and property testing

### Mandatory regression scenarios

Restricted account claiming Plexamp.

Restricted account selecting prohibited media index.

Restricted account with no media index where 4K and 1080p both exist.

Unknown direct part request.

Restricted account requesting a part previously seen by unrestricted account.

Account without library access requesting a section hub cached by another account.

Account without item access requesting artwork cached by another account.

Opaque token spoofing another client identifier.

Malformed media index, bitrate, buffer size, content type header, and Plex metadata.

Identity API outage under fail open and fail closed modes.

Full redirect behaviour matrix.

### Fuzz and property targets

`disk_cache::decode_record`

Resolution parsing and classification

Query parameter parsing

Canonical cache key generation

Plex JSON parsing

Plex XML parsing where accepted

Every account scoped cache key function should have a property asserting different token inputs produce different keys while identical token plus canonical request produces the same key.

## Phase 15: Strengthen CI

Once current dependency advisories are clean, remove `|| true` from `cargo audit` so advisories fail CI.

Add `cargo deny` for advisories, licences, sources, and duplicate dependency policy.

Add `rust-toolchain.toml` to pin the release toolchain.

Consider Dependabot or Renovate for Cargo and GitHub Actions dependencies.

Add a CI test configuration with resolution policy explicitly enabled so protected routes cannot disappear unnoticed.

Continue multi architecture image builds.

## Phase 16: Improve observability without leaking credentials

Add structured fields for request ID, path, username, verified UUID, identity source, token fingerprint prefix, effective resolution limit, bitrate cap, requested media index, selected media index, stream transport mode, cache hit/miss/stale state, and policy rejection reason.

Never log raw Plex tokens or full authorisation headers.

Useful metrics include identity resolution latency, identity source distribution, cache hit ratios, upstream Plex latency, upstream errors, policy rejects, direct part rejects, proxy versus redirect counts, disk cache size, and eviction count.

## Phase 17: Documentation and deployment hardening

Document that Plex origin isolation is required for hard resolution enforcement.

Document the exact redirect matrix.

Document token fingerprint fallback as the preferred solution for opaque PMS tokens.

Document client identifier and username fallback as weaker compatibility modes if retained.

Document that account scoped caches intentionally trade some memory for correct authorisation behaviour.

Remove stale strict stream guard documentation.

Ensure production Docker examples do not expose the Plex origin to normal clients when demonstrating hard enforcement.

The recommended deployment should expose Replex or the Cloudflare tunnel while keeping the Plex origin on an internal network for client traffic.

## Recommended implementation order

Complete the Plexamp and Live TV bypass fix first.

Then secure fallback identity binding.

Then account scope hub caches.

Then account scope Plex artwork caches.

Then correct stream redirect semantics and remove dead strict guard configuration.

Then harden the disk cache.

Then introduce shared application state and persistent HTTP clients.

Then introduce request scoped security context.

Then refactor transforms.

Then split large modules.

Finally complete broad panic removal, CI strengthening, observability, and documentation cleanup.

The first four phases should be treated as security patches and should not be mixed with large mechanical refactors.

## Suggested commit boundaries

One commit for mandatory security handling before Plexamp and Live TV compatibility behaviour.

One commit for token fingerprint identity binding and identity regression tests.

One commit for account scoped hub caches and cross account tests.

One commit for account scoped photo caches and cross account tests.

One commit for stream redirect correctness and strict guard cleanup.

One commit for disk cache safety.

One commit for shared application state and HTTP clients.

One commit for request scoped security context.

One commit for transform pipeline improvements.

Module splitting should then be performed as mostly mechanical commits with minimal behavioural change.

## Release strategy

After the first four security phases, deploy to a test Replex instance with at least one unrestricted account and one restricted account.

Exercise Plex Web, iOS or Android, TV clients, direct play, direct stream, transcode, resume playback, Continue Watching, section hubs, images, and direct part requests.

Run an explicit Plex Experience Preview hero compatibility matrix against
Plex Web, Apple TV/tvOS Preview, Android TV Preview, iOS Preview, Android
mobile Preview, and Roku Preview where available. For Continue Watching and
custom hero collections, capture the upstream PMS payload, the transformed
Replex payload, and the artwork actually rendered by the client. Verify that
Replex preserves Plex-native `Image` entries such as `coverPoster`,
`background`, `clearLogo`, and `backgroundSquare` while upserting only the
hero `coverArt` entry. Confirm each client honours the expected
`Meta.displayImages`/`coverArt` combination and document any platform-specific
`thumb` or `art` fallback that remains necessary.

Confirm restricted clients never receive a Plex origin redirect.

Confirm unrestricted clients follow the configured redirect mode.

Confirm account B cannot retrieve section metadata or artwork cached by account A when Plex would not authorise B for the source library.

Compare selected media versions against Plex server and Replex policy logs.

After HTTP client pooling changes, measure latency against the previous build before changing cache TTLs or timeouts.

## Definition of done

The Plexamp and Live TV bypass is removed.

Opaque identity fallback is securely token bound.

Authenticated hub metadata is account scoped.

Plex library artwork cannot cross account cache boundaries.

Restricted stream bytes always remain behind Replex.

Unrestricted streams respect `REPLEX_REDIRECT_STREAMS`.

Dead security configuration is removed or explicitly deprecated.

Disk cache corruption and concurrent writers cannot crash or corrupt the service.

HTTP clients and configuration are shared through application state.

Identity and effective policy are resolved once per protected request.

Transform filtering avoids owned metadata cloning and optional key unwrap assumptions.

Every identified bypass has an adversarial regression test.

CI enforces dependency security once the existing advisory baseline is clean.

Continue Watching hero transformation is covered by regression tests and does
not discard Plex-native artwork metadata.

The Plex Experience Preview compatibility matrix has been exercised for the
supported client families before a hero-style release is declared compatible.

Documentation accurately describes runtime behaviour and the network security boundary.

## Final architecture direction

```text
Incoming request
        |
Parse request context
        |
Determine whether protected account context is required
        |
Resolve verified or securely bound account identity
        |
Resolve effective policy once
        |
Store RequestSecurityContext
        |
Authorisation aware cache lookup
        |
Mandatory playback or metadata policy enforcement
        |
Optional client compatibility and presentation transforms
        |
Shared long lived upstream HTTP client when a fetch is required
        |
Cache raw account scoped data where appropriate
        |
Return response
```

The ordering above is the key architectural objective. Security must occur before compatibility behaviour and before any cache shortcut that could otherwise bypass Plex account permissions.
