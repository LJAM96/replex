# Phase 0 Checklist — Per-User Resolution Restrictions

Sign-off sheet for Phase 0 of `implementation-plan.md`. Complete every item
before starting Phase 1. Nothing in this phase changes code or server state.

## 1. Account topology

- [ ] Restricted users (e.g. the 1080p-only account) are **invited shared
      accounts**, not home/managed users.
      Check: they appear in Plex Settings → Friends, or run
      `PLEX_TOKEN=<admin token> ./scripts/list_shared_users.sh`
- [ ] Usernames and account UUIDs recorded for every user needing a policy:

      | User | Username | UUID | Intended limit |
      |------|----------|------|----------------|
      | (admin) | | | unlimited |
      | | | | |
      | | | | |

- [ ] Each restricted user's own Plex token works against this server
      (ask them to test playback via a direct URL, or verify during Stage B).

## 2. Network control

- [x] GDM can be disabled on the Plex server (Settings → Network → "Enable
      local network discovery (GDM)" → off / firewall blocks UDP 32410-32414).
- [~] Shared users cannot reach the Plex server directly — **accepted
      residual risk**: the server's public endpoint (`:42442`) cannot be
      firewalled because the operator does not provide that control.
      Enforcement via Replex covers all normal use; a determined user who
      deliberately connects direct can bypass. Mitigations: Custom server
      access URL steers clients to Replex; deliberate evasion is treated as a
      trust issue, not a technical one.
- [ ] Plan exists to disable Remote Access for shared users at cutover
      (Phase 7 Stage C).
- [ ] Custom server access URL can be set in Plex settings
      (Settings → Network → "Custom server access URL") and points at the
      planned Replex HTTPS endpoint.
- [ ] HTTPS hostname + certificate available for the Replex endpoint
      (e.g. `replex-test.example.com`).

**Note:** with item 2 partially unmet, enforcement is advisory against
deliberate bypass. This was reviewed and **accepted** on 2026-08-21.

## 3. Baseline capture

Prerequisites: stock (unmodified) Replex running and reachable,
admin Plex token.

- [ ] Run:
      ```bash
      REPLEX_BASE_URL=http://<replex-host>:<port> PLEX_TOKEN=<admin-token> \
          ./scripts/capture_baseline.sh
      ```
- [ ] Identify a movie that has **both** 1080p and 4K versions; re-run with
      `MOVIE_RATING_KEY=<key>` so `movie_detail.json` shows two `Media`
      entries.
- [ ] Confirm captured files exist under `tests/mock/out/baseline/`:
      - [ ] `library_sections` (.json + .xml)
      - [ ] `library_sections_all.json`
      - [ ] `movie_detail` (.json + .xml) — contains ≥2 Media versions
      - [ ] `episode_detail` (.json + .xml)
      - [ ] `hubs_promoted.json`, `hubs_home.json`
      - [ ] `playback_decision.json`, `playback_start.json`,
            `playback_decision_mediaindex.json`
- [ ] Commit the baseline directory.
- [ ] Sanity check later runs:
      ```bash
      REPLEX_BASE_URL=... PLEX_TOKEN=... ./scripts/capture_baseline.sh --verify
      ```
      must report `OK: baseline matches` while Replex is unmodified.

## Sign-off

| Item | Date | By |
|------|------|----|
| Topology confirmed | | |
| Network control confirmed | | |
| Baseline captured + committed | | |

Once signed off, proceed to Phase 1 (policy engine foundation).
