# Review resolution status

**Updated 2026-08-27, after fixes against `main` (base review commit `5ab90c3`).**

## Priority outcomes

- **P0 (all resolved).** `PART_POLICY_CACHE` is keyed by
  `(verified user uuid, policy fingerprint, part id)`; Continue Watching /
  On Deck / home / promoted hubs are user-scoped by token hash, with warmer
  parity and unit tests (commit `0c34d9f`). The enforcement question was
  decided: resolution limits are **convenience, not enforcement** — this is
  now stated explicitly in `README.md` and `docker/portainer-stack.yml`
  (including the stream-redirect caveat), so the direct-origin bypass is a
  documented property of the deployment rather than an undocumented
  contradiction.
- **P1 (all resolved).**
  1. Persistent library caching redesigned: the disk cache stores the RAW
     upstream payload (`library_cache_store`), and `library_cache_lookup`
     re-runs `apply_policy_transforms()` with the requesting account's
     current policy on every hit. Corrupt entries are evicted and refetched
     (`disk_cache::remove`). `disk_cache::put` no longer drifts the size
     counter on overwrite.
  2. Library warmer key mismatch fixed: one canonical function
     (`routes::library_cache_key_for`) is used by both the request path and
     the warmer; client-shaping noise (`X-Plex-*` metadata, field-trimming
     params) is canonicalized away and misses fetch normalized supersets,
     so warmed entries are consumed by real client requests. Keys are
     user-scoped by token SHA-256 (raw library payloads embed per-account
     watch state).
  3. Cross-user integration tests added:
     `cross_user_library_sections_isolation` (unlimited vs 1080p account
     through the real router: per-account filtering, no shared cache scope,
     restrictions survive cache hits), `policy_change_is_honoured_on_library_
     disk_cache_hits` (the "Monday 4K / Tuesday 1080p" scenario), plus
     warmer/request key-parity and fetch-normalization tests.
     `tests/direct_parts.rs` already covered the cross-user part guard.

## Verification

Full suite green at time of writing: 51 tests passed, 0 failed (47 lib
tests + playback, identity, direct_parts integration suites). Clippy clean
on all files touched by these fixes.

## Still open (P2)

Atomic disk-cache writes (tmp + rename), persisted photo content type for
disk hits, removing the `/replex/test_proxy` route from release builds,
tightening permissive CORS, panic-path hardening for hostile headers/params,
CI running fmt/clippy/test before Docker publishing, Docker `USER` /
healthcheck hardening.

---

I reviewed the current `main` branch at commit `5ab90c3129dbbee881946f8016b9756f39c71af7`, committed on 27 August 2026. 

My overall assessment is that the fork has moved Replex in a genuinely useful direction. The per account resolution work is substantially better designed than I expected, the identity verification logic is thoughtful, and the caching work is targeting the right bottlenecks. However, I would **not currently treat the resolution restrictions as a reliable access control boundary**. There are several concrete cross user caching issues that need fixing first.

## Most important findings

• **HIGH: `PART_POLICY_CACHE` can allow one user's permissions to affect another user.**

This is the most serious implementation bug I found.

The cache is:

`Cache<i64, bool>`

where the key is only the Plex media `part_id`. The boolean is calculated using the policy of whichever user caused that media to be evaluated. 

The direct stream guard subsequently does:

`PART_POLICY_CACHE.get(&part_id)`

and trusts the resulting boolean. 

Consider:

```text
User A
limit = 4K

part 123 = 4K
cache[123] = true

User B
limit = 1080p

requests part 123

cache[123] = true

request permitted
```

That means a 4K permitted account can effectively prime a 4K part for a 1080p restricted account.

`REPLEX_STRICT_STREAM_GUARD=true` does not solve this because the part is no longer unknown.

I would replace this architecture rather than simply extend the key. Ideally the stream request should independently determine whether that part belongs to media permitted by the authenticated user's current policy.

If a cache is necessary, use something conceptually like:

```rust
(user_uuid, policy_fingerprint, part_id)
```

rather than:

```rust
part_id
```

This should be the first thing fixed.

## HIGH: Continue Watching is being shared between users

The current hub caching model assumes that the raw Plex hub response can be shared between accounts.

`cached_hubs_response()` deliberately constructs a cache key without the user's Plex token. 

That can make sense for genuinely global discovery rows.

It does **not** make sense for Continue Watching.

The background warmer explicitly fetches:

```text
/hubs/continueWatching
```

using the configured administrator token. 

That response is then stored in the common hub cache.

So under normal warming behaviour:

```text
Admin Plex token
        ↓
Plex Continue Watching
        ↓
Shared hub cache
        ↓
User A
User B
User C
```

Continue Watching is account specific. It cannot safely be cached this way.

The downstream `HubWatchedTransform` does not rebuild another user's Continue Watching list. It only removes already present entries based on state. 

I would divide hubs into two cache classes.

```text
GLOBAL

Recently Added
Global collection hubs
Library discovery rows
Static promoted content where confirmed user independent


USER SCOPED

Continue Watching
On Deck
Anything dependent on watch state
Anything dependent on account restrictions
Anything Plex generates differently by user
```

User scoped entries should use a SHA256 token hash or verified user UUID in the cache key.

This is both a correctness problem and potentially a privacy problem because viewing activity from one Plex account could appear on another account.

## HIGH: The persistent library cache can preserve old permissions

The new disk cache is persistent and has no expiration metadata. A cached file survives until storage pressure causes LRU eviction. 

The `/library/sections/...` cache is positioned before the resolution transform.

On a miss it allows the remaining pipeline to run, then stores the resulting body.

On a hit it returns the stored body and calls `skip_rest()`. 

That means you are effectively persisting the **already authorised and transformed response**.

For example:

```text
Monday

Luke
limit = 4K

/library/sections/23/all
cached to disk


Tuesday

Luke
limit changed to 1080p

same URL
same token
same disk cache key

old 4K filtered response returned
resolution transform skipped
```

The same problem applies to changes in hidden collections and potentially library contents.

The better architecture already exists elsewhere in Replex. Your hub cache stores the raw source response and performs account specific transforms after retrieving it. 

I would adopt exactly that pattern for library caching:

```text
Plex
 ↓
raw shared or appropriately scoped cache
 ↓
current request
 ↓
identity resolution
 ↓
current policy
 ↓
transform
 ↓
client
```

Do not make the persistent cache an authority on permissions.

## HIGH: The new library warmer currently cannot hit its own cache entries

This is a straightforward bug in the latest commit.

The warmer creates a key shaped approximately as:

```rust
library:{section_id}:{start}:50:{token_prefix}
```

while the request handler looks for:

```rust
library:{token_prefix}:{raw_path_and_query}
```

Those keys cannot match.  

So the new library prewarming feature can successfully fetch and write data to disk while live requests never consume those warmed entries.

There should be one canonical function responsible for library keys, used by both the warmer and request path. Add a test that constructs the same request through both paths and asserts identical keys.

## HIGH architectural issue: Redirecting streams weakens the entire security model

Your README correctly states that if clients can access Plex directly they can bypass Replex restrictions. 

The supplied Portainer deployment then enables:

```text
REPLEX_REDIRECT_STREAMS=true
```

and explicitly says video bytes are sent directly to the bare metal Plex server. 

This creates an unavoidable distinction.

If the 1080p limitation is intended as a convenience feature, this is fine.

If it is intended as **enforcement**, the Plex origin cannot also be directly reachable by the restricted user.

Once the client has the origin and a Plex token accepted by that origin, Replex cannot reliably stop that user constructing direct Plex requests.

For hard enforcement I would require:

```text
Internet
 ↓
Replex
 ↓
Plex

Client ✕ Plex directly
```

The Plex media server should ideally only accept relevant remote traffic from Replex, or streaming itself needs to continue through an enforcing gateway.

This is more fundamental than any Rust implementation detail.

## MEDIUM: The disk cache needs more robustness

There are several issues in `disk_cache.rs`. 

`CURRENT_SIZE` increases every time `put()` executes, even when an existing cache file is overwritten. The old length is never subtracted. The figure therefore drifts upwards until a full scan eventually corrects it.

Writes go directly to the final file. A crash or concurrent read can encounter an incomplete response. Cache writes should use a temporary file followed by an atomic rename.

`walkdir` and `filetime::set_file_mtime()` are synchronous filesystem operations executed inside async functions. A very large cache scan can occupy a Tokio worker thread.

The cache size is hard coded to 45 GiB. This should be configurable alongside the existing cache settings.

Also, a disk photo cache hit is always returned as:

```text
Content-Type: image/jpeg
```

even though the in memory cache preserves the original content type. 

If Plex returns WebP, PNG or another format, the persisted version is now semantically different from the original.

I would persist a small cache record containing body, content type, created timestamp, last accessed timestamp and potentially an ETag or source generation value.

## MEDIUM: Too many externally reachable `unwrap()` paths

There is still a lot of historical Replex code that assumes Plex clients always send perfectly formed values.

For example, `Config::dynamic()` unwraps the Host header and then performs several unchecked Base32 operations for `replex.stream`. 

`deserialize_screen_resolution()` parses and indexes user supplied resolution strings using `unwrap()` and direct array indexing. 

`PlexClient::from_context()` expects a token and panics if there is none. 

A service exposed through Cloudflare should consider every header and query parameter hostile or malformed.

These should become `Result` paths returning 400, 401 or 403 as appropriate rather than panicking.

## MEDIUM: A development proxy route is shipped in the production router

The production router contains:

```text
/replex/test_proxy/<anything>
```

and the handler targets:

```text
https://webhook.site
```



The proxy implementation is not conditionally compiled for tests. 

I would remove this route completely from release builds.

Even aside from any possible header forwarding, a production service should not contain an externally accessible arbitrary testing path targeting a third party service.

The global router also currently uses permissive CORS. 

That deserves tightening now that Replex is intended to be Internet accessible.

## CI needs attention

There are actually a reasonable number of tests in the Rust source. The resolution classifier in particular has good tests for 1080p, ultrawide 4K, 8K, unknown media and policy matching. 

The problem is that the workflows are primarily **building**, not validating.

The release workflow performs release builds but does not run `cargo test`, `cargo clippy` or `cargo fmt --check`. 

The PR workflow also uses `pull_request_target`, a `PERSONAL_TOKEN`, logs into GHCR and pushes images. 

I would change the ordinary PR pipeline to:

```text
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test --all-targets
cargo audit or cargo deny
docker build without push
```

Then keep image publishing exclusively for trusted pushes to `main`.

The `main` branch is also currently unprotected and has no required status checks. 

## Docker can be hardened

The runtime container starts the Replex binary as root because no `USER` is declared. 

There is no reason for the HTTP service to need root after startup.

I would give it a dedicated unprivileged UID, make the filesystem read only apart from `/data`, add a healthcheck and pin deployment images to explicit versions where practical.

The Portainer example also currently contains a real looking Plex address and account usernames in comments. 

I would replace those with generic examples in a public repository.

## What I think is good

The central resolution classification logic is good. It takes the more restrictive interpretation when the textual resolution and actual dimensions disagree, blocks unknown media for restricted users and handles ultrawide 4K correctly. 

Identity resolution is also substantially better than simply trusting `X-Plex-Username`. Tokens are hashed for the identity cache, Plex is queried for the real account, shared users have additional resolution paths, and the unsafe username fallback is opt in. 

The fail closed default is the right default for this feature. 

The transform model is also a good architectural idea. The problem is primarily that the newer caches sometimes sit on the wrong side of those transforms.

## Code structure

I would now refactor before adding many more features.

`routes.rs` is about 87 KB and `models.rs` about 60 KB. The routing file currently contains routing, playback decisions, cache handling, stream redirects, policy enforcement, webhooks, proxy implementation glue and tests. 

I would move toward roughly:

```text
auth/
policy/
playback/
proxy/
cache/
    hub.rs
    library.rs
    image.rs
routes/
    hubs.rs
    library.rs
    playback.rs
    images.rs
```

Most importantly, I would establish one rule throughout the application:

**Caches may accelerate data retrieval, but a cached value must never itself represent an authorisation decision made for another request.**

That single principle would prevent both of the biggest bugs I found.

### Priority

My order of work would be:

**P0:** Fix `PART_POLICY_CACHE`.

**P0:** Stop sharing Continue Watching and other user specific hubs.

**P0:** Decide whether resolution limiting is supposed to be real enforcement. If yes, remove direct origin bypass.

**P1:** Redesign persistent library caching so policy transforms always execute using the current account and current configuration.

**P1:** Fix the warmer key mismatch.

**P1:** Add explicit cross user integration tests with one unlimited or 4K account and one 1080p account.

**P2:** Harden parsing, remove production test routes, tighten CORS and run unprivileged.

**P2:** Make CI actually run the test suite before Docker publishing.

With those corrected, I think the underlying design is good enough to build on. The most concerning problems are concentrated around the newer multi user caching and stream enforcement rather than the basic Replex transform architecture.