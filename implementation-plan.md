# Replex Per-User Resolution Restrictions — Implementation Plan

Companion to `resretrict.md` (design + feasibility review). This plan turns the
design into ordered, verifiable work from first commit to production cutover.

Ground rules carried over from the feasibility review:

* Everything is gated behind `REPLEX_RESOLUTION_POLICY_ENABLED` (default
  `false`). With the flag off, behaviour must be byte-identical to stock.
* Identity comes only from the request's `X-Plex-Token`, never from username
  headers.
* Playback enforcement is the real guarantee; metadata filtering is UX only.

---

## Phase 0 — Preconditions (no code)

Tooling for this phase is already in place:

* `phase0-checklist.md` — the sign-off sheet for items 1 and 2 below.
* `scripts/list_shared_users.sh` — lists shared accounts with username + UUID
  (admin token required) for building the policy table.
* `scripts/capture_baseline.sh` — captures all Phase 3 regression fixtures
  (JSON + XML) into `tests/mock/out/baseline/`; supports `--verify` mode that
  diffs live responses against the committed baseline.

1. Confirm Plex account topology:
   * Restricted users are **invited shared accounts** (not home/managed).
   * Collect each user's username and account UUID via
     `./scripts/list_shared_users.sh`.
2. Confirm network control is achievable:
   * GDM can be disabled on the server.
   * Shared users cannot reach Plex directly (firewall/VPN plan exists).
   * You can set a Custom server access URL in Plex settings.
3. Baseline capture before any changes:
   * Run
     `REPLEX_BASE_URL=<replex> PLEX_TOKEN=<admin> ./scripts/capture_baseline.sh`
     against **stock** Replex.
   * Re-run with `MOVIE_RATING_KEY=<key>` pointing at a movie that has both
     1080p and 4K versions so `movie_detail.json` contains two `Media`
     entries.
   * Commit `tests/mock/out/baseline/`; later use `--verify` to prove stock
     behaviour hasn't drifted.

**Exit gate:** checklist complete; fixtures committed.

---

## Phase 1 — Policy engine foundation

Files: `src/config.rs`, new `src/resolution_policy.rs`.

Tasks:

1. Config fields (follow existing figment patterns in `src/config.rs`):
   * `resolution_policy_enabled: bool` (default false)
   * `user_resolution_policies: Vec<PolicyEntry>` (custom serde deserializer,
     JSON array; entries support `username` and/or `uuid`)
   * `resolution_default: ResolutionLimit` (default `unlimited`)
   * `resolution_policy_fail_closed: bool` (default true)
   * `strict_stream_guard: bool` (default false)
2. Types:
   * `enum ResolutionLimit { P480, P720, P1080, P2160, Unlimited }`
   * `struct UserIdentity { id: i64, uuid: String, username: String }`
   * `struct ResolutionPolicy { limit: ResolutionLimit }`
   * `fn classify(media: &Media) -> Option<ResolutionLimit>` — uses
     `videoResolution` string AND `width`/`height`; more restrictive wins;
     unknown → `None`.
   * `fn media_allowed(media: &Media, policy) -> bool` (`None` classification
     = not allowed for restricted users)
   * `fn allowed_media(&[Media], policy) -> Vec<Media>`
   * `fn best_allowed_media(&[Media], policy, screen_res) -> Option<Media>`
3. Unit tests for classification exactly per `resretrict.md` (1920x1080,
   1920x800, 3840x2160, 3840x1608, 4096x2160, 7680x4320 cases).

**Exit gate:** `cargo test` green; no routing or proxy behaviour changed;
flag-off config parses identically to today.

---

## Phase 2 — Identity resolution + cache

Files: `src/plex_client.rs`, `src/resolution_policy.rs`.

Tasks:

1. Add `PlexClient::get_current_user(&self) -> Result<UserIdentity>`:
   * Calls `GET https://plex.tv/api/v2/user` with the request token
     (`X-Plex-Token` header) — **new code**, nothing to reuse here.
   * Distinguishes 401 (bad token → clear error kind) from other failures.
2. Identity cache:
   * Key: hash of token (never the raw token), TTL 60 min.
   * On 401: evict immediately, propagate auth failure.
   * Never log tokens; scrub from error chains.
3. Token extraction helper shared by all handlers: checks `X-Plex-Token`
   header **and** query parameter (some clients use query only).
4. Policy lookup: match resolved identity by uuid first, then username; fall
   back to `resolution_default`.
5. Tests (mock plex.tv via the existing mock-server harness):
   valid restricted / unrestricted / invalid / expired / missing tokens;
   spoofed username header ignored; query-param token works.

**Exit gate:** unit + integration tests green; manual check that two different
shared-account tokens resolve to different identities against real Plex.

---

## Phase 3 — Metadata filtering transform

Files: new `src/transform/resolution_policy.rs`, `src/transform/mod.rs`,
`src/routes.rs`.

Tasks:

1. Implement `ResolutionPolicyTransform` modelled on
   `src/transform/restrictions.rs`:
   * `transform_metadata`: remove prohibited `Media` entries recursively
     (movies, episodes under shows/directories).
   * `filter_metadata`: return false when an item had media but none survive
     (hides 4K-only items).
   * Resolve identity once per response, not per item (use `PlexContext`).
2. Route wiring — the big one:
   * Wrap `/library/metadata/<id>` and `/library/sections/**` responses in the
     transform pipeline (`proxy_for_transform`), plus `/playQueues`.
   * Only parse when content type is JSON/XML metadata; pass everything else
     through untouched.
   * Keep `Container` size fields consistent after removals (existing
     `apply_to` already fixes `size`).
3. Regression fixtures from Phase 0 must still pass unchanged for
   unrestricted users.

**Exit gate:** restricted test account sees no 4K in browse/detail/search/hubs;
unrestricted responses byte-compare equal to Phase 0 fixtures; latency of
library browse measured and within ~10% of baseline.

---

## Phase 4 — Playback enforcement

Files: `src/routes.rs`, `src/resolution_policy.rs`.

Tasks:

1. New hoop `enforce_resolution_policy`, inserted **first** on decision,
   start, and subtitles routers (`src/routes.rs:82-100` region):
   * If policy disabled or user unrestricted → no-op (zero overhead path).
   * If `mediaIndex` present and allowed → leave unchanged.
   * If `mediaIndex` present and prohibited → rewrite to best allowed index
     using `get_item_by_key` + `best_allowed_media`.
   * If no mediaIndex → filter candidate list so downstream auto-selection
     (`auto_select_version`) only ever sees allowed versions.
   * If nothing allowed → `403`.
2. Refactor `auto_select_version` to accept a pre-filtered media list instead
   of raw item media (single selection system, per design).
3. Make `video_transcode_fallback` filter candidates through `allowed_media()`
   before choosing a fallback version.
4. Verify ordering: policy → auto-select → force-max-quality → transcode
   fallback → direct-stream fallback.
5. Tests: manual 4K `mediaIndex` rewritten; auto-select matrix (4K user +
   1080p client → 1080p; 1080p user + 4K client → 1080p); transcode fallback
   never crosses the limit; subtitles router honours policy.

**Exit gate:** with metadata filtering temporarily disabled, a restricted
account still cannot start the 4K version by any request shape tried.

---

## Phase 5 — Direct part protection + stream redirect ordering

Files: `src/routes.rs`.

Tasks:

1. Restructure the `/library/parts/<itemid>/<partid>/file.<ext>` handler
   (currently goes straight to `redirect_stream`, `src/routes.rs:45-50`):
   * Resolve identity → load item → find Media containing partid → apply
     policy → then redirect/proxy.
   * Prohibited or unresolvable identity with `strict_stream_guard=true` →
     `403`.
2. Same ordering inside the transcode-session redirect path
   (`/video/:/transcode/universal/session/...`): authenticate and check before
   issuing any redirect.
3. Confirm VPS mode stays light: video bytes are redirected, not relayed.

**Exit gate:** direct 4K part URL replay as restricted user returns 403;
1080p part plays normally; redirect behaviour unchanged for admin.

---

## Phase 6 — Hardening, docs, release

Tasks:

1. Logging per design doc (debug for normal flow, info for violations, warn
   for identity failures; never log tokens). Add a log-based smoke assertion
   in tests where practical.
2. Fail-closed behaviour tests: kill mock plex.tv mid-test, confirm protected
   requests fail closed while unrestricted-configured users recover from
   cache.
3. README: configuration reference, examples for the Luke/Jodie case, VPS
   deployment, strict mode, network-lockdown requirements, unsupported
   home/managed users.
4. Version bump, changelog, build Docker image, tag pre-release.

**Exit gate:** CI green (fmt, clippy, tests); image published.

---

## Phase 7 — Staged production rollout

### Stage A — Shadow deployment (no user impact)

* Run new image beside existing service on a separate port/host
  (`replex-test.example.com`, HTTPS).
* Point **only your own admin account** at it (Custom access URL test or
  manual app config).
* Run Phase 0 fixture comparisons live; watch logs for a week of normal use.

### Stage B — Restricted pilot

* Enable policies; add the 1080p-only user's policy.
* Have that user test the full client checklist from `resretrict.md`
  (Plex Web → Apple TV → iOS → Android TV): no 4K offered, play/resume/
  transcode/search/collections all work.
* Attempt bypasses yourself: cached deep link, direct part URL, forced
  `mediaIndex`, XML vs JSON clients.

### Stage C — Full cutover

* Add remaining policies (4K user etc.).
* Network lockdown **where possible**: disable GDM, move Custom server access
  URL to Replex, disable Remote Access for shared users. The Plex server's
  public port cannot be firewalled (no operator control) — this residual
  bypass risk was reviewed and **accepted** during Phase 0: enforcement is
  complete for all normal client behaviour; a user who deliberately connects
  to the direct public endpoint can circumvent the policy.
* Keep old service running idle for rollback for ≥2 weeks.

### Rollback plan (any stage)

* Set `REPLEX_RESOLUTION_POLICY_ENABLED=false` and restart → stock behaviour
  instantly (identities/policies ignored).
* Worst case: repoint clients/DNS to previous Replex instance or direct Plex;
  restriction lifts but nothing breaks.

### Post-production monitoring

* First week: daily review of warn/info policy-violation logs and identity
  failure warnings.
* Watch for: unexpected 403s from legit users (identity cache/Plex API
  issues), clients hitting Plex directly (bypass attempts show up as plays
  absent from Replex logs — cross-check Plex dashboard).

---

## Dependency graph

```text
Phase 0 ─→ Phase 1 ─→ Phase 2 ─→ Phase 3 ─┬─→ Phase 4 ─→ Phase 5 ─→ Phase 6 ─→ Phase 7
                                           │
              (Phase 4 can start once Phase 2 lands; Phases 3 and 4 are
               independent and can proceed in parallel after Phase 2)
```

## Effort estimate (rough)

| Phase | Estimate |
|---|---|
| 0 Preconditions | half day |
| 1 Policy engine | 1–2 days |
| 2 Identity | 1–2 days |
| 3 Metadata routes | 3–5 days (highest risk) |
| 4 Playback enforcement | 2–3 days |
| 5 Part protection | 1–2 days |
| 6 Hardening/docs | 1–2 days |
| 7 Rollout | 1–2 weeks elapsed (soak time) |

Total: roughly 3–4 weeks calendar including soak, ~10–17 working days of
implementation.
