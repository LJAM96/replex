
# Replex Per User Resolution Restrictions

## Goal

Extend Replex so individual Plex accounts can have a maximum permitted media resolution while continuing to use the same Plex libraries and the same merged Plex items.

The intended behaviour is:

```text
Movie
    1080p version
    4K version
```

For a restricted user:

```text
Movie
    1080p version
```

For an unrestricted or 4K user:

```text
Movie
    1080p version
    4K version
```

The restriction must be based on the authenticated Plex account, not the device being used.

A 1080p restricted account must not be able to bypass the restriction by manually selecting the 4K version.

## Core requirements

* Keep the existing Plex Movies and TV libraries unchanged.

* Keep 1080p and 4K files merged under the same Plex item.

* Apply resolution rules according to the authenticated Plex account.

* Support independent Plex accounts that receive shared library access.

* Hide prohibited versions from Plex clients.

* Prevent prohibited versions from being selected during playback.

* Prevent Plex fallback logic from accidentally selecting a prohibited version.

* Preserve current Replex behaviour for users without a configured restriction.

* Make the feature disabled by default so existing Replex installations behave exactly as before.

* Work when Replex is hosted on a separate VPS.

* Avoid sending actual video data through the VPS when stream redirection is enabled.

## Feasibility review

This plan was reviewed against the actual Replex codebase. Overall verdict:
**feasible and not hallucinated**, with two corrections and several caveats.
The core mechanism works because Replex is a transparent reverse proxy that
forwards each client's own `X-Plex-Token` upstream
(`src/plex_client.rs:431`), so the proxy always knows which shared account is
making a request, and Plex accepts shared users' tokens against the server.

Confirmed feasible:

| Claim | Verified |
|---|---|
| Client tokens are forwarded per-request | Yes — `src/plex_client.rs:431`, cache keys include token (`:416`) |
| `auto_select_version` skips explicit `mediaIndex` | Yes — `src/routes.rs:1247-1252` |
| `Media` model has `videoResolution`, `width`, `height` | Yes — `src/models.rs:556-586` |
| Transform framework supports recursive filtering and item removal | Yes — `filter_metadata` / `transform_metadata` in `src/transform/mod.rs:57-72` |
| Transcode fallback iterates versions independently of policy | Yes — `src/routes.rs:1048+`, must be filtered as planned |
| Stream redirection exists and can stay VPS-friendly | Yes — `redirect_stream` routes in `src/routes.rs:37-51` |

Corrections to earlier claims:

1. **No existing Plex user API code.** The claim that Replex already contacts
   the Plex user API with the request token is false. Only
   `discover.provider.plex.tv`, `notifications.plex.tv` and `clients.plex.tv`
   are contacted today. `get_current_user` is new work against
   `https://plex.tv/api/v2/user`. Straightforward, but new.

2. **Metadata routes are not transformed today.** `/library/metadata/*` and
   `/library/sections/*` fall through to plain `proxy_request`
   (`src/routes.rs:188`). Wiring them into the transform pipeline is the
   largest piece of work and the main regression risk.

Known limitations (must be accepted, not solved by this feature):

1. **Network-level bypass.** This enforces only traffic that flows through
   Replex. A client that discovers Plex directly via GDM, the Plex relay, or
   the `clients.plex.tv` resource list can connect around the proxy entirely.
   Enforcement therefore requires: GDM disabled on the server, Plex not
   directly reachable from shared users' networks, Remote Access disabled (or
   those users blocked), and Replex set as the Custom server access URL.
   **Status:** the hosted Plex server's public port cannot be firewalled (no
   operator control). This residual risk was reviewed and **accepted**
   (2026-08-21): enforcement is complete for normal client behaviour;
   deliberate direct connection is treated as a trust issue.

2. **Home/managed users.** Managed (home) accounts authenticate differently
   from invited shared users and may not resolve cleanly via
   `/api/v2/user` with their own token. First release should target invited
   shared accounts only and document home users as unsupported.

3. **Client-side caching.** Clients cache metadata; a restricted user may
   briefly see stale 4K entries until refresh. Playback enforcement (not
   metadata hiding) is what actually guarantees the restriction — which the
   plan already treats as the real enforcement layer.

4. **`strict_stream_guard` is undefined.** It appears in the configuration and
   deployment sections but its behaviour is specified nowhere. Either define
   it (recommended meaning: reject any `/library/parts` request whose identity
   cannot be resolved when the policy feature is enabled) or remove it.

Estimated effort: the policy engine, identity resolution and playback
enforcement are modest (a few hundred lines each). The metadata route wiring,
client compatibility testing across XML/JSON responses, and the network-lockdown
deployment work dominate the real cost.

## Codebase integration points

Concrete anchors for implementers (verified against this repo):

* **Config pattern** — all settings live in `src/config.rs` as figment env
  fields (`REPLEX_` prefix) using the
  `#[serde(default = "default_as_false", deserialize_with = "figment::util::bool_from_str_or_int")]`
  pattern (`src/config.rs:46-49`). New flags should follow it exactly. JSON
  list config can follow the `deserialize_comma_seperated_string` precedent or
  a custom serde deserializer for `Vec<ResolutionPolicyEntry>`.
* **Transform example** — `src/transform/restrictions.rs` is the closest
  existing model for `ResolutionPolicyTransform`: it implements
  `filter_metadata`, reads config per-call, and uses
  `plex_client.get_cached(...)` for upstream lookups.
  `ResolutionPolicyTransform` should resolve identity via `PlexContext`
  (already passed to every transform call) rather than per-item Plex calls.
* **Item lookup** — `PlexClient::get_item_by_key` (`src/plex_client.rs:272`)
  already fetches an item with its media versions and is used by both
  `auto_select_version` (`src/routes.rs:1263`) and
  `video_transcode_fallback` (`src/routes.rs:1076`). Playback enforcement can
  reuse it directly.
* **Caching** — `PlexClient::get_cached` / `CACHE` (`src/cache.rs`) is a
  process-wide TTL cache; the identity cache should either reuse it with a
  distinct key prefix (`identity:<token-hash>`) or be a separate small
  moka-style cache so identity entries can be invalidated on auth failure.
* **Handler wiring** — playback hoops are attached in `route()`
  (`src/routes.rs:82-100`) in explicit order; policy enforcement must be
  inserted as the first hoop on decision/start/subtitles routers.
* **Query rewriting** — `replace_query(queries, req)` at the end of
  `auto_select_version` (`src/routes.rs:1320`) is the established way to
  mutate request queries before proxying; `enforce_resolution_policy` should
  use the same mechanism.
* **XML + JSON** — models use dual serde/yaserde derives, so transforms
  operating on parsed `MediaContainer` structs automatically cover both
  formats; no format-specific filtering code is needed.
* **Tests** — `tests/server.rs` and `src/test_helpers/mod.rs` use a mock
  upstream server (`get_mock_server()`); new route tests should extend that
  harness with fixture responses containing multi-version media.

## Proposed configuration

Add a new feature flag:

```text
REPLEX_RESOLUTION_POLICY_ENABLED=true
```

Add user policies through JSON:

```text
REPLEX_USER_RESOLUTION_POLICIES=[
  {
    "username": "user1080",
    "max_resolution": "1080"
  },
  {
    "username": "user4k",
    "max_resolution": "4k"
  }
]
```

Supported limits initially:

```text
480
720
1080
4k
unlimited
```

Add:

```text
REPLEX_RESOLUTION_DEFAULT=unlimited
```

This controls users who do not have an explicit rule.

For your use case:

```text
Your account
    unlimited

User A
    1080

User B
    4k
```

Username should only be used as the configuration identifier.

It must not be trusted directly from the incoming `X Plex Username` value.

## Authenticated user identification

Create a `UserIdentity` model containing:

```rust
struct UserIdentity {
    id: i64,
    uuid: String,
    username: String,
}
```

When a request arrives:

```text
Plex client
     ↓
X Plex Token
     ↓
Replex
     ↓
Plex user API
     ↓
Verified Plex account
     ↓
Resolution policy
```

> **Feasibility correction:** an earlier draft claimed Replex already contains
> code which contacts the Plex user API using the request token. This is **not
> true**. A search of the codebase confirms the only `plex.tv` calls are
> `discover.provider.plex.tv` (`src/plex_client.rs:360`),
> `notifications.plex.tv` (`src/routes.rs:376`) and `clients.plex.tv`
> (`src/routes.rs:387`). There is no existing account verification anywhere.
> `get_current_user` must be written from scratch against
> `https://plex.tv/api/v2/user` (or `/api/home/users` for home users), sent
> with the *request's* token. This is straightforward — the request token is
> already available on every request via `PlexContext` — but budget it as new
> work, not a refactor.

What does exist and matters:

* Replex forwards each client's own `X-Plex-Token` upstream
  (`src/plex_client.rs:431`), so shared users' individual tokens reach both
  Replex and Plex. Token based identity resolution is therefore genuinely
  possible.
* The response cache key already includes the token
  (`src/plex_client.rs:416`), so per-user caching fits the existing design.

Add a reusable method to `PlexClient`, for example:

```rust
async fn get_current_user(&self) -> Result<UserIdentity>
```

The returned Plex account becomes the authoritative identity.

The incoming username header can still be logged for debugging but must never determine the policy on its own.

## User identity cache

Do not query Plex for the account on every request.

Add a memory cache:

```text
Plex token
    ↓
Verified UserIdentity
```

Recommended expiry:

```text
60 minutes
```

Do not write Plex tokens to logs.

Do not expose tokens in errors.

A token that receives an authentication failure should immediately have its cached identity removed.

> **Token location:** some clients send the token as a query parameter rather
> than the `X-Plex-Token` header (see `tests/artilleryws.yml` for an example).
> Identity extraction must check both header and query consistently, and the
> playback/decision handlers must use the same extraction path.

## Stable account identifiers

Internally policies should support both:

```text
username
uuid
```

Username makes initial configuration easy.

UUID gives a stable identifier if the account changes its username.

Once Replex resolves a user, log something similar to:

```text
Resolution identity resolved
username=user1080
uuid=xxxxxxxx
```

This allows the administrator to migrate the configuration to UUID later.

Eventually the preferred configuration could become:

```json
[
  {
    "uuid": "USER_UUID",
    "max_resolution": "1080"
  }
]
```

## Resolution policy engine

Create a new module:

```text
src/resolution_policy.rs
```

It should contain all resolution decisions instead of spreading them throughout `routes.rs`.

Suggested types:

```rust
enum ResolutionLimit {
    P480,
    P720,
    P1080,
    P2160,
    Unlimited,
}
```

Add helpers similar to:

```rust
fn media_allowed(
    media: &Media,
    policy: &ResolutionPolicy
) -> bool
```

and:

```rust
fn allowed_media(
    media: &[Media],
    policy: &ResolutionPolicy
) -> Vec<Media>
```

and:

```rust
fn best_allowed_media(
    media: &[Media],
    policy: &ResolutionPolicy,
    screen_resolution: Option<Resolution>
) -> Option<Media>
```

Every part of Replex that makes a version decision should use these shared functions.

## Resolution detection

Do not rely solely on the string:

```text
videoResolution
```

Use both Plex's resolution value and the actual dimensions.

For a 1080p restriction:

```text
width <= 1920
height <= 1080
```

For 4K:

```text
width <= 4096
height <= 2160
```

This is important for widescreen 4K encodes that could be something such as:

```text
3840 x 1608
```

Checking only the height would incorrectly treat that as below 1080p.

The Plex `videoResolution` field should also be recognised:

```text
480
576
720
1080
2k
4k
6k
8k
```

If the textual resolution and dimensions disagree, use the more restrictive interpretation.

For a restricted user, media whose resolution cannot be determined should not automatically be treated as acceptable.

## Metadata filtering

Create:

```text
src/transform/resolution_policy.rs
```

Add:

```rust
ResolutionPolicyTransform
```

The transform should inspect:

```rust
MetaData.media
```

and remove any `Media` entries which exceed the authenticated user's limit.

Before:

```text
Metadata
    Media 1080p
    Media 4K
```

After processing for a 1080p user:

```text
Metadata
    Media 1080p
```

For an unrestricted user no changes are made.

## Recursive filtering

Do not only process the top level `MediaContainer`.

Plex responses can contain nested:

```text
Metadata
Video
Directory
```

structures.

Create a reusable recursive function that walks all metadata children and filters their `media` arrays.

This avoids having different behaviour between:

```text
Library view
Movie details
Season view
Episode view
Search results
Home hubs
Collections
```

## Items that only exist in 4K

This case needs explicit behaviour.

Suppose the library contains:

```text
Movie A
    1080p
    4K

Movie B
    4K only
```

For a 1080p restricted user:

```text
Movie A
    visible

Movie B
    hidden
```

If an item originally contained media versions but none survive the policy filter, remove that item from the response wherever practical.

If the user reaches the item directly using a previously cached Plex URL, playback must still be blocked.

Do not transcode the 4K file down to 1080p as a substitute.

The purpose of this policy is to prevent the user from accessing the 4K source, not merely limit output resolution.

## Plex metadata routes

Resolution filtering should be applied to responses containing media metadata.

Initial route coverage should include:

```text
/library/metadata/*
/library/sections/*
/hubs/*
/playQueues
```

Rather than duplicating filtering logic for every route, create a common metadata response filtering handler.

Only responses which can contain Plex metadata should be parsed.

Binary stream data, images and unrelated requests should continue through the existing proxy path untouched.

> **Feasibility correction:** today only the hub routes
> (`PLEX_HUBS_PROMOTED`, `/hubs/home`, `/hubs/sections/<id>`) and
> `/replex/<style>/*` pass responses through transforms
> (`src/routes.rs:126-181`). `/library/metadata/*` and `/library/sections/*`
> currently fall through to plain `proxy_request`
> (`src/routes.rs:188`) with no response parsing at all. Adding metadata
> filtering to those routes means wiring them into `proxy_for_transform` and
> parsing every browse/detail/search response. This is the single largest and
> riskiest piece of the plan — it changes the hot path for all library
> browsing, so it needs careful performance testing (the existing `CACHE`
> should be reused where possible) and regression testing across clients.

## Playback enforcement

Metadata hiding is only the user interface layer.

The real restriction must happen during playback.

The current `auto_select_version` function specifically skips processing when a Plex client explicitly provides `mediaIndex`.

That behaviour must change for restricted accounts.

> **Verified in code:** `auto_select_version` returns early when `mediaIndex`
> is present and not `-1` (`src/routes.rs:1247-1252`). This is the bypass hole.
> Note also that the same handler is attached to the subtitles router
> (`src/routes.rs:86`), so policy enforcement must be applied there too, or a
> restricted user can still trigger version lookups against the 4K stream
> context.

Create:

```text
enforce_resolution_policy
```

For every playback request:

```text
Determine authenticated user

Load user policy

Load Plex item

Read available media versions

Determine requested mediaIndex

Check whether requested version is allowed
```

If the requested version is acceptable:

```text
leave mediaIndex unchanged
```

If the requested version exceeds the account limit:

```text
replace mediaIndex with the best permitted version
```

For example:

```text
User limit
1080p

Requested
mediaIndex=1

Index 1
4K

Index 0
1080p
```

Replex changes the request to:

```text
mediaIndex=0
```

The client should therefore simply start the 1080p version.

## No permitted version

If there is no permitted version:

```text
403 Forbidden
```

should be returned for the playback request.

Do not silently permit the higher resolution version.

Do not silently transcode it.

## Integration with automatic version selection

Do not maintain two completely separate selection systems.

Refactor the existing version selection logic to use the policy engine.

Conceptually:

```text
All media versions
        ↓
Apply user resolution policy
        ↓
Allowed versions
        ↓
Apply client resolution preference
        ↓
Best version
        ↓
mediaIndex
```

This preserves the useful current Replex behaviour while enforcing the account limit.

For example:

```text
User maximum
4K

Client
1080p television

Available
1080p
4K
```

Result:

```text
1080p
```

Another device using the same account:

```text
User maximum
4K

Client
4K television
```

Result:

```text
4K
```

For the restricted account:

```text
User maximum
1080p

Client
4K television
```

Result:

```text
1080p
```

The account policy always wins over device capability.

## Transcode fallback integration

The existing `video_transcode_fallback` logic also iterates through available versions.

It must use the same:

```rust
allowed_media()
```

helper.

Otherwise this could happen:

```text
1080p user starts 1080p version

1080p version requires transcode

Replex fallback searches alternatives

Replex chooses 4K
```

That would bypass the policy.

The fallback candidate list must therefore be filtered before any fallback selection takes place.

## Force maximum quality

`force_maximum_quality` can remain supported.

The execution order should ensure the version policy runs before any quality modification.

Recommended order:

```text
Automatic version preference

Resolution policy enforcement

Force maximum quality

Video transcode fallback

Direct stream fallback
```

The transcode fallback itself must also be policy aware.

## Direct media access protection

Metadata filtering and `mediaIndex` enforcement cover normal Plex clients.

For stronger enforcement, also protect direct media part requests.

Replex already handles paths similar to:

```text
/library/parts/{itemid}/{partid}/file.ext
```

> **Feasibility note:** this route currently goes straight to
> `redirect_stream` with no authentication or user resolution at all
> (`src/routes.rs:45-50`). Implementing part validation requires restructuring
> that handler so identity resolution and the policy check happen before any
> redirect is issued.

Before redirecting or proxying the file:

```text
Resolve the Plex item

Find the Media containing partid

Resolve authenticated user

Apply resolution policy
```

If the part belongs to a prohibited version:

```text
403 Forbidden
```

If permitted:

```text
continue normally
```

This prevents someone who already knows the direct 4K part URL from bypassing the restriction.

## Stream redirection

The existing Replex stream redirection feature should remain compatible.

The order must be:

```text
Request
   ↓
Authenticate
   ↓
Resolution check
   ↓
Allowed?
   ↓
Redirect stream
```

Never redirect first and attempt to validate afterwards.

This allows Replex to run on your VPS without requiring the VPS to relay the entire movie.

Replex already supports redirecting media streams to another endpoint. 

## Failure behaviour

Add:

```text
REPLEX_RESOLUTION_POLICY_FAIL_CLOSED=true
```

For configured resolution restrictions, I recommend failing closed.

If Replex cannot verify the Plex account identity:

```text
do not accidentally give unrestricted access
```

Return a temporary service error for protected media requests instead.

Identity caching should mean temporary Plex API problems rarely affect playback.

For users explicitly configured as unrestricted, normal behaviour can continue once their identity is verified.

## Logging

Add useful structured messages.

Examples:

```text
Resolution policy matched
username=user1080
maximum=1080
```

```text
Media versions filtered
rating_key=1234
before=2
after=1
```

```text
Blocked media version
username=user1080
requested=4k
maximum=1080
replacement=1080
```

Never log:

```text
X Plex Token
```

Normal successful requests should use debug level where possible to avoid noisy logs.

Policy violations should use info level.

Unexpected identity or policy failures should use warning level.

## Files to modify

### src/config.rs

Add:

```text
resolution_policy_enabled
user_resolution_policies
resolution_default
resolution_policy_fail_closed
strict_stream_guard
```

Add configuration parsing.

`strict_stream_guard` (previously undefined): when `true` and the resolution
policy feature is enabled, any `/library/parts` request whose authenticated
identity cannot be resolved is rejected with `403` instead of being redirected.
When `false`, unresolvable identities fall back to current behaviour.

### src/models.rs

Only add general models here if they genuinely belong to Plex protocol modelling.

Do not put the entire policy implementation into this already large file.

### src/resolution_policy.rs

New file.

Contains:

```text
ResolutionLimit
ResolutionPolicy
UserIdentity
resolution classification
policy lookup
allowed_media
best_allowed_media
```

### src/plex_client.rs

Add reusable verified user lookup.

Add user identity caching.

Reuse the current request token.

### src/transform/resolution_policy.rs

New transform.

Filters prohibited `Media` versions from metadata.

Filters items which contain no permitted media.

Supports nested metadata.

### src/transform/mod.rs

Register and export the new transform.

### src/routes.rs

Add the policy middleware.

Apply metadata filtering routes.

Modify automatic version selection.

Modify transcode fallback candidate selection.

Protect direct part access.

### README.md

Document configuration.

Document separate Plex account behaviour.

Document 1080p and 4K examples.

Document VPS use.

Document strict mode.

## Tests

The policy engine should have unit tests before routing changes are added.

### Resolution classification

Test:

```text
1920 x 1080
allowed for 1080

1920 x 800
allowed for 1080

3840 x 2160
blocked for 1080

3840 x 1608
blocked for 1080

4096 x 2160
allowed for 4K

7680 x 4320
blocked for 4K
```

### Metadata filtering

Input:

```text
Movie
    1080p
    4K
```

1080 user result:

```text
Movie
    1080p
```

4K user result:

```text
Movie
    1080p
    4K
```

### 4K only item

Input:

```text
Movie
    4K
```

1080 user result:

```text
item hidden
```

### Manual version selection

Send:

```text
mediaIndex=4K_INDEX
```

as a 1080 restricted account.

Expected:

```text
mediaIndex rewritten to 1080_INDEX
```

### Automatic selection

Test:

```text
4K user
1080 client
result 1080
```

```text
4K user
4K client
result 4K
```

```text
1080 user
4K client
result 1080
```

### Fallback

Force the 1080 version to require transcoding.

Confirm a 1080 user is never moved to the 4K version.

### Direct file access

Request the 4K `partid` as the restricted user.

Expected:

```text
403
```

Request the 1080 `partid`.

Expected:

```text
allowed
```

### Authentication

Test:

```text
valid restricted token
valid unrestricted token
invalid token
expired token
missing token
spoofed username with another user's token
token sent as query parameter instead of header
home/managed account token (expected: unsupported, fail closed)
```

A spoofed username must never override the identity associated with the Plex token.

Identity extraction must work whether the token arrives in the `X-Plex-Token`
header or the query string, since some clients use only one of the two.

### Formats

Test both:

```text
application/json
XML
```

because different Plex clients may request different response formats.

## Client testing

Test at minimum:

```text
Plex Web
Apple TV Plex client
iPhone or iPad Plex client
Android TV if available
```

For each restricted account verify:

```text
Movie page does not offer 4K

Play Version does not expose 4K

Normal Play chooses 1080p

Direct Play works

Transcoding works without moving to 4K

Resume works

Continue Watching works

Search results work

Collections work

TV episodes work
```

For the 4K account verify the normal Plex experience is unchanged.

## VPS deployment strategy

Do not replace the current Plex connection immediately.

Run the modified Replex on a separate port first:

```text
Plex
    existing service

VPS
    Replex test instance
```

Expose it temporarily through HTTPS using a separate hostname.

For example:

```text
replex-test.example.com
```

Test using the administrator account first.

Then test using the 1080 restricted Plex account.

Then test the 4K account.

Only after version filtering and playback enforcement are confirmed should the Replex address become the Plex Custom server access URL.

Keep the existing Plex Remote Access arrangement available during initial testing so there is an easy recovery path.

Once Replex is proven stable, clients can be directed through the proxy.

> **Required network lockdown:** the policy is only as strong as the requirement
> that all client traffic flows through Replex. Before enabling enforcement:
>
> * Disable GDM on the Plex server.
> * Make Plex unreachable directly from shared users' networks (firewall).
> * Disable Remote Access for shared users, or block it.
> * Set Replex as the Custom server access URL so `clients.plex.tv` advertises
>   only the proxy address.

## Deployment configuration

An eventual container configuration would conceptually contain:

```yaml
environment:
  REPLEX_HOST: "https://existing-plex-server"
  REPLEX_TOKEN: "ADMIN_TOKEN"

  REPLEX_RESOLUTION_POLICY_ENABLED: "true"

  REPLEX_USER_RESOLUTION_POLICIES: >
    [
      {
        "username": "user1080",
        "max_resolution": "1080"
      },
      {
        "username": "user4k",
        "max_resolution": "4k"
      }
    ]

  REPLEX_RESOLUTION_DEFAULT: "unlimited"

  REPLEX_RESOLUTION_POLICY_FAIL_CLOSED: "true"

  REPLEX_STRICT_STREAM_GUARD: "true"

  REPLEX_REDIRECT_STREAMS: "true"
```

The real Plex token should of course be provided through an appropriate secret mechanism rather than committed to the compose file.

## Acceptance criteria

The feature is complete when all of the following are true.

* A 1080p restricted Plex account sees the same Movies and TV libraries as before.

* A movie containing both 1080p and 4K appears only once.

* The restricted account sees only the 1080p version.

* The restricted account cannot manually select the 4K version.

* A direct attempt to access the prohibited 4K part is rejected when strict protection is enabled.

* A 4K permitted account sees both versions.

* An unrestricted account behaves exactly as stock Replex.

* A 4K capable television does not override an account level 1080p restriction.

* Transcode fallback cannot cross the user's maximum resolution.

* Titles containing only 4K media are unavailable to a 1080p account.

* Movies and TV episodes both behave consistently.

* Stream redirection continues to work so the VPS does not have to relay the video payload.

* Disabling `REPLEX_RESOLUTION_POLICY_ENABLED` restores original Replex behaviour.

## Recommended implementation sequence

### Foundation

Add the policy models, configuration parser and resolution classifier.

No routing changes yet.

Complete unit tests.

### Identity

Extract Plex account verification into `PlexClient`.

Add identity caching.

Verify independent shared Plex accounts resolve to different identities.

### Metadata

Implement `ResolutionPolicyTransform`.

Apply it to movie and episode metadata responses.

Verify the 4K option disappears from the restricted account.

### Playback

Add resolution enforcement to playback decision and start requests.

Refactor automatic version selection to use the allowed media list.

### Fallback

Make video transcode fallback policy aware.

Audit every other location that changes `mediaIndex`.

### Strict protection

Add direct `partid` validation.

Ensure validation happens before stream redirection.

### Client compatibility

Test Plex Web first.

Then Apple TV and mobile clients.

Capture any additional Plex API routes that need metadata transformation.

### VPS rollout

Build the Docker image.

Deploy beside the existing service.

Use HTTPS.

Test with separate Plex accounts.

Move the Plex Custom server access URL to the Replex endpoint only after all acceptance tests pass.

## Scope for the first release

Keep the first version deliberately focused.

Implement:

```text
Account based resolution limits
Metadata version filtering
Playback enforcement
Fallback enforcement
Direct part protection
VPS compatible deployment
```

Do not initially add:

```text
Web administration interface
Database
User management portal
Automatic library restructuring
Sonarr integration
Radarr integration
Tautulli dependency
Complex device rules
Bitrate quotas
HDR restrictions
Codec restrictions
```

Those can be added later because the policy engine would already provide the foundation for rules such as:

```text
maximum bitrate
HDR allowed
Dolby Vision allowed
maximum audio channels
remote stream quality
device specific restrictions
```

The first release should solve one problem reliably: **a Plex account configured for 1080p must behave as though the 4K versions simply do not exist.**
