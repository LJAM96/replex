//! Stale-while-revalidate layer for hub payloads.
//!
//! Hub responses (`/hubs/*`) are served from the shared raw-payload cache.
//! This module tracks *when* each payload was fetched and keeps it fresh:
//!
//! - Past `hub_stale_ttl`, a cached payload is still served immediately but
//!   a single-flight background refresh is kicked off, so no client ever
//!   blocks on a (potentially very slow) upstream fetch.
//! - Playback changes observed through the proxy (`/:/scrobble`,
//!   `/:/timeline` with state=stopped) mark every hub entry stale. The next
//!   request then serves instantly while refreshing behind the scenes, which
//!   keeps Continue Watching / On Deck accurate within seconds of activity.

use crate::models::*;
use crate::plex_client::PlexClient;
use crate::utils::from_reqwest_response;
use crate::config::Config;
use crate::routes::PHOTO_CACHE;
use moka::future::Cache;
use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Fetch time per `hubcache:` key. No entry == unknown age == stale.
static HUB_FETCHED_AT: Lazy<Cache<String, Instant>> = Lazy::new(|| {
    let c: Config = Config::figment().extract().unwrap();
    Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(c.cache_ttl))
        .build()
});

/// Last background refresh attempt per key. Rate-limits refreshes so a
/// stream of playback events cannot hammer an upstream that is slow to
/// regenerate promoted payloads (observed 4s-50s).
static HUB_LAST_REFRESH_ATTEMPT: Lazy<Cache<String, Instant>> =
    Lazy::new(|| Cache::builder().max_capacity(10_000).build());

const REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(60);

/// Cache keys with a background refresh currently running.
static REFRESH_INFLIGHT: Lazy<Mutex<HashSet<String>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

pub(crate) async fn track_fetched(cache_key: String) {
    HUB_FETCHED_AT.insert(cache_key, Instant::now()).await;
}

/// Hub payloads that Plex generates per account. These must never be served
/// from a cache entry fetched with another account's token:
/// - `continueWatching` / `ondeck` are derived entirely from that account's
///   watch state.
/// - `home` and `promoted` embed Continue Watching / On Deck rows whose
///   *membership* is account specific; the per-user transforms can prune
///   entries but cannot rebuild another account's rows.
/// Everything else (e.g. `/hubs/sections/<id>` discovery rows such as
/// Recently Added) is genuinely shared and stays in the common cache.
pub fn is_user_scoped_hub(key_path: &str) -> bool {
    let lower = key_path.to_ascii_lowercase();
    lower.contains("continuewatching")
        || lower.contains("ondeck")
        || lower.contains("/hubs/home")
        || lower.contains("/hubs/promoted")
}

/// Short hash of a token used as the user-scope component of cache keys.
/// Raw tokens never appear in cache keys.
fn hub_user_scope(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    data_encoding::HEXLOWER.encode(&digest)[..16].to_string()
}

/// Build the canonical hub cache key for a request path.
///
/// User-scoped hubs embed a hash of the requesting token so each account
/// reads only payloads fetched with its own credentials; all other hubs
/// share one entry (per-user transforms still run after retrieval).
/// The warmer must build keys through this same function so warmed entries
/// land where the matching requests look them up.
pub fn hub_cache_key(key_path: &str, token: Option<&str>) -> String {
    if is_user_scoped_hub(key_path) {
        match token {
            Some(t) => format!("hubcache:u:{}:{}", hub_user_scope(t), key_path),
            None => format!("hubcache:u:anon:{key_path}"),
        }
    } else {
        format!("hubcache:{key_path}")
    }
}

/// Pure staleness decision so tests can inject timestamps.
fn is_older_than(fetched_at: Option<Instant>, max_age: Duration, now: Instant) -> bool {
    match fetched_at {
        Some(t) => now.duration_since(t) >= max_age,
        None => true,
    }
}

pub(crate) async fn is_stale(cache_key: &str, max_age: Duration) -> bool {
    is_older_than(
        HUB_FETCHED_AT.get(cache_key).await,
        max_age,
        Instant::now(),
    )
}

/// Mark every cached hub payload stale by dropping its age record. Payloads
/// stay in place so requests keep being served instantly while a background
/// refresh repopulates them.
pub(crate) fn mark_all_hubs_stale() {
    HUB_FETCHED_AT.invalidate_all();
}

/// Kick off (or skip, if already running/rate limited) a single-flight
/// background refresh for one cached hub payload. Never blocks the caller.
pub(crate) fn spawn_hub_refresh(client: PlexClient, key_path: String, cache_key: String) {
    tokio::spawn(async move {
        if let Some(t) = HUB_LAST_REFRESH_ATTEMPT.get(&cache_key).await {
            if t.elapsed() < REFRESH_MIN_INTERVAL {
                return;
            }
        }
        if !claim_refresh(&cache_key) {
            return;
        }
        HUB_LAST_REFRESH_ATTEMPT
            .insert(cache_key.clone(), Instant::now())
            .await;

        refresh_hub_entry(&client, &key_path, &cache_key).await;

        let mut inflight = REFRESH_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        inflight.remove(&cache_key);
    });
}

fn claim_refresh(cache_key: &str) -> bool {
    let mut inflight = REFRESH_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    inflight.insert(cache_key.to_string())
}

/// Re-fetch one hub payload from upstream and swap it into the cache. On any
/// failure the previous payload is kept: stale data beats no data.
async fn refresh_hub_entry(client: &PlexClient, key_path: &str, cache_key: &str) {
    tracing::debug!(path = %key_path, "background hub refresh started");
    match fetch_hubs_payload(client, key_path).await {
        Ok(payload) => {
            client.cache.insert(cache_key.to_string(), payload).await;
            track_fetched(cache_key.to_string()).await;
            tracing::debug!(path = %key_path, "background hub refresh done");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %key_path,
                "background hub refresh failed, keeping previous payload"
            );
        }
    }
}

/// Fetch a raw hub payload from upstream, transparently falling back to
/// `/hubs/promoted` when the server has removed `/hubs/home`.
pub(crate) async fn fetch_hubs_payload(
    client: &PlexClient,
    key_path: &str,
) -> Result<MediaContainerWrapper<MediaContainer>, anyhow::Error> {
    let r = client.get(key_path.to_string()).await.map_err(|e| {
        tracing::warn!(error = %e, path = %key_path, "upstream hub fetch failed");
        salvo::http::StatusError::bad_gateway()
    })?;

    // Recent Plex Media Server versions removed /hubs/home (upstream 404).
    // Serve the promoted payload instead so legacy clients keep a working
    // home screen. Servers that still provide /hubs/home are unaffected:
    // the fallback only triggers on a 404.
    let r = if r.status() == reqwest::StatusCode::NOT_FOUND
        && key_path.starts_with("/hubs/home")
    {
        let fallback_path = key_path.replacen("/hubs/home", "/hubs/promoted", 1);
        tracing::info!(
            path = %key_path,
            fallback = %fallback_path,
            "upstream /hubs/home unavailable, serving promoted hubs"
        );
        client.get(fallback_path).await.map_err(|e| {
            tracing::warn!(
                error = %e,
                path = %key_path,
                "promoted fallback fetch failed"
            );
            salvo::http::StatusError::bad_gateway()
        })?
    } else {
        r
    };

    if r.status() != reqwest::StatusCode::OK {
        tracing::warn!(status = %r.status(), path = %key_path, "upstream hub fetch non-200");
        return Err(salvo::http::StatusError::bad_gateway().into());
    }
    let parsed = from_reqwest_response(r).await.map_err(|e| {
        tracing::warn!(error = ?e, path = %key_path, "hub payload parse failed");
        salvo::http::StatusError::bad_gateway()
    })?;
    Ok(parsed)
}

/// Does this proxied request change data that hubs display?
///
/// Deliberately narrow: `/:/timeline` fires roughly every 10 seconds during
/// playback with state=playing/paused/buffering, and those pings must not pin
/// every hub permanently stale. Watched-state flips (`/:/scrobble`,
/// `/:/unscrobble`) and playback stopping are the events that materially
/// change Continue Watching membership and ordering; everything else ages out
/// through the normal staleness window.
pub(crate) fn is_playback_invalidation(path: &str, query: Option<&str>) -> bool {
    if path.ends_with("/:/scrobble") || path.ends_with("/:/unscrobble") {
        return true;
    }
    if path.ends_with("/:/timeline") {
        let state = query.and_then(|q| {
            q.split('&').find_map(|p| p.strip_prefix("state="))
        });
        return matches!(state, Some("stopped"));
    }
    false
}

// ---------------------------------------------------------------------------
// Background warmer
//
// The first request after boot for a hub payload is a slow blocking upstream
// fetch (observed 45s-2min for merged promoted payloads), and clients render
// an empty home screen rather than wait. Warming the canonical keys with the
// admin token on a timer keeps every client load on the fast cache/SWR path.
// ---------------------------------------------------------------------------

/// The canonical hub paths to keep warm, derived from the server's library
/// sections. Mirrors what real clients request: one promoted key per section
/// slot (web clients page through them individually), the aggregate slot,
/// continue watching and the /hubs/home fallback.
async fn warm_paths(host: &str, token: &str) -> Vec<String> {
    let sections_url = format!("{}/library/sections", host.trim_end_matches('/'));
    tracing::debug!(url = %sections_url, "warmer listing sections");
    let resp = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .get(&sections_url)
        .header("Accept", "application/json")
        .header("X-Plex-Token", token)
        .send()
        .await
    {
        Ok(r) => {
            tracing::debug!(status = %r.status(), "warmer sections response");
            r
        }
        Err(e) => {
            tracing::warn!(error = %e, "warmer could not list library sections");
            return vec![];
        }
    };
    #[derive(serde::Deserialize)]
    struct Sections {
        #[serde(rename = "Directory", default)]
        directory: Vec<Section>,
    }
    #[derive(serde::Deserialize)]
    struct Section {
        key: String,
    }
    let all = match resp.json::<crate::models::MediaContainerWrapper<Sections>>().await {
        Ok(s) => {
            let dirs: Vec<String> = s
                .media_container
                .directory
                .into_iter()
                .map(|d| d.key)
                .collect();
            tracing::debug!(count = dirs.len(), "warmer parsed sections");
            dirs
        }
        Err(e) => {
            tracing::warn!(error = %e, "warmer could not parse library sections");
            return vec![];
        }
    };
    if all.is_empty() {
        tracing::warn!("warmer found no library sections");
        return vec![];
    }
    let joined = all.join(",");
    let mut paths = vec![
        format!("/hubs/promoted?contentdirectoryid={joined}&pinnedcontentdirectoryid={joined}"),
        "/hubs/home".to_string(),
        format!("/hubs/continueWatching?contentdirectoryid={joined}"),
    ];
    for s in &all {
        paths.push(format!(
            "/hubs/promoted?contentdirectoryid={s}&pinnedcontentdirectoryid={joined}"
        ));
    }
    paths
}

/// One warmer pass: fetch every canonical path that is missing or stale.
async fn warm_cycle() {
    let config: Config = Config::figment().extract().unwrap();
    if config.warm_interval == 0 {
        return;
    }
    let Some(token) = config.token.clone() else {
        tracing::debug!("warmer idle: no admin token configured");
        return; // no admin token configured, nothing to warm with
    };
    let Some(host) = config.host.clone() else {
        return;
    };
    tracing::debug!("hub warmer cycle starting");

    let context = crate::models::PlexContext {
        token: Some(token.clone()),
        client_identifier: Some("replex-warmer".to_string()),
        ..Default::default()
    };
    let client = PlexClient::from_context(&context);

    let mut warmed: Vec<(String, crate::models::MediaContainerWrapper<crate::models::MediaContainer>)> = Vec::new();

    for path in warm_paths(&host, &token).await {
        let fetch_path = if path.contains('?') {
            format!("{}&includeGuids=1&count=50", path)
        } else {
            format!("{}?includeGuids=1&count=50", path)
        };
        // Compute the cache key with the exact same logic client requests
        // go through, so warmer entries land where lookups land. Passing
        // the admin token keeps user-scoped hubs (Continue Watching, home,
        // promoted) consistent with admin requests.
        let cache_key = {
            let mut probe = salvo::http::Request::default();
            let uri: hyper::Uri = fetch_path.parse().expect("valid warm path");
            probe.set_uri(uri);
            crate::hub_cache::hub_cache_key(
                &crate::routes::cache_key_for_request(&probe),
                Some(&token),
            )
        };
        if !claim_refresh(&cache_key) {
            continue;
        }
        match fetch_hubs_payload(&client, &fetch_path).await {
            Ok(payload) => {
                client.cache.insert(cache_key.clone(), payload.clone()).await;
                track_fetched(cache_key.clone()).await;
                warmed.push((fetch_path.clone(), payload));
                tracing::debug!(path = %fetch_path, "warmed hub payload");
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %fetch_path, "warm fetch failed");
            }
        }
        let mut inflight = REFRESH_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        inflight.remove(&cache_key);
    }

    // Poster prefetch: pre-transcode thumbs from the warmed rows so a
    // user's FIRST view of home is fully warm too. The photo cache is
    // shared across users; skip anything already cached.
    const POSTERS_PER_ROW: usize = 12;
    const POSTER_BUDGET: usize = 400;
    let mut budget = POSTER_BUDGET;
    let mut fetched = 0usize;
    'outer: for (_, payload) in warmed.iter() {
        let mut payload = payload.clone();
        for hub in payload.media_container.children_mut() {
            for item in hub.children().iter().take(POSTERS_PER_ROW) {
                if budget == 0 {
                    break 'outer;
                }
                let Some(thumb) = item.thumb.as_deref() else {
                    continue;
                };
                let tq = match build_transcode_query(thumb) {
                    Some(q) => q,
                    None => continue,
                };
                let key =
                    crate::routes::canonical_photo_key(&format!("/photo/:/transcode?{tq}"));
                if PHOTO_CACHE.get(&key).await.is_some() {
                    continue;
                }
                match client.get(tq.clone()).await {
                    Ok(r) if r.status() == reqwest::StatusCode::OK => {
                        let ct = r
                            .headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        match r.bytes().await {
                            Ok(bytes) if bytes.len() <= 4 * 1024 * 1024 => {
                                PHOTO_CACHE.insert(
                                    key.clone(),
                                    crate::routes::CachedImage {
                                        content_type: ct.clone(),
                                        cache_control: Some(
                                            "public, max-age=259200".to_string(),
                                        ),
                                        body: bytes.to_vec(),
                                    },
                                )
                                .await;
                                let _ = crate::disk_cache::put(&key, &bytes).await;
                                budget -= 1;
                                fetched += 1;
                            }
                            _ => {}
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "poster prefetch failed");
                    }
                }
            }
        }
    }
    if fetched > 0 {
        tracing::info!(count = fetched, "poster prefetch complete");
    }

    warm_library_pages(&client, &host, &token).await;
}

async fn warm_library_pages(client: &PlexClient, host: &str, token: &str) {
    let sections = {
        let url = format!("{}/library/sections", host.trim_end_matches('/'));
        let resp = match reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap()
            .get(&url).header("Accept", "application/json").header("X-Plex-Token", token).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::OK => r,
            _ => return,
        };
        #[derive(serde::Deserialize)] struct Sections { #[serde(rename = "Directory", default)] directory: Vec<Section> }
        #[derive(serde::Deserialize)] struct Section { key: String }
        match resp.json::<crate::models::MediaContainerWrapper<Sections>>().await {
            Ok(s) => s.media_container.directory.into_iter().map(|d| d.key).collect::<Vec<_>>(),
            Err(_) => return,
        }
    };
    if sections.is_empty() { return; }
    for section in &sections {
        let section_id = section.as_str();
        for start in (0..250).step_by(50) {
            let path = format!("/library/sections/{}/all?X-Plex-Container-Start={}&X-Plex-Container-Size=50", section_id, start);
            let cache_key = format!("library:{}:{}:50:{}", section_id, start, &token[..8.min(token.len())]);
            if crate::disk_cache::get(&cache_key).await.is_some() {
                continue;
            }
            let url = format!("{}{}", host.trim_end_matches('/'), path);
            let resp = match reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap()
                .get(&url)
                .header("Accept", "application/json")
                .header("X-Plex-Token", token)
                .send()
                .await
            {
                Ok(r) if r.status() == reqwest::StatusCode::OK => r,
                Ok(r) => {
                    tracing::debug!(status = %r.status(), path = %path, "library warm non-200");
                    break;
                }
                Err(e) => {
                    tracing::debug!(error = %e, path = %path, "library warm failed");
                    break;
                }
            };
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(_) => break,
            };
            let _ = crate::disk_cache::put(&cache_key, &bytes).await;
            tracing::debug!(section = %section_id, start = start, bytes = bytes.len(), "warmed library page");
            tokio::time::sleep(Duration::from_millis(200)).await;
            if bytes.len() < 1000 {
                break;
            }
        }
    }
}

/// Build a canonical poster transcode query for a thumb path, matching
/// the dimensions Plex Web requests so warmed entries land on the exact
/// cache keys clients look up.
fn build_transcode_query(thumb: &str) -> Option<String> {
    let mut url = url::Url::parse("http://x/photo/:/transcode").ok()?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("width", "240");
        pairs.append_pair("height", "360");
        pairs.append_pair("minSize", "1");
        pairs.append_pair("upscale", "1");
        pairs.append_pair("url", thumb);
    }
    url.query().map(|q| q.to_string())
}

/// Start the background warmer loop. Runs forever; call once at startup.
pub fn spawn_warmer() {
    tokio::spawn(async move {
        // Give the server a moment to bind before spending upstream time.
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            warm_cycle().await;
            let interval: u64 = {
                let c: Config = Config::figment().extract().unwrap();
                c.warm_interval.max(1)
            };
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex_client::PlexClient;

    #[test]
    fn user_scoped_hub_classification() {
        // Account-specific payloads must be user scoped.
        assert!(is_user_scoped_hub("/hubs/continueWatching"));
        assert!(is_user_scoped_hub(
            "/hubs/continueWatching?contentdirectoryid=1,2"
        ));
        assert!(is_user_scoped_hub("/hubs/onDeck"));
        assert!(is_user_scoped_hub("/hubs/home"));
        assert!(is_user_scoped_hub("/hubs/promoted?contentdirectoryid=23"));
        // Library discovery rows are genuinely shared.
        assert!(!is_user_scoped_hub("/hubs/sections/6"));
        assert!(!is_user_scoped_hub("/library/sections/6/all"));
    }

    #[test]
    fn user_scoped_hubs_get_per_token_keys() {
        let global = hub_cache_key("/hubs/sections/6", Some("tokenA"));
        assert_eq!(global, "hubcache:/hubs/sections/6");

        let a1 = hub_cache_key("/hubs/continueWatching", Some("tokenA"));
        let a2 = hub_cache_key("/hubs/continueWatching", Some("tokenA"));
        let b = hub_cache_key("/hubs/continueWatching", Some("tokenB"));
        assert_eq!(a1, a2, "same token must hit the same entry");
        assert_ne!(
            a1, b,
            "different accounts must never share Continue Watching"
        );
        assert!(a1.starts_with("hubcache:u:"));
        assert!(!a1.contains("tokenA"), "raw tokens must not appear in keys");

        let anon = hub_cache_key("/hubs/continueWatching", None);
        assert_eq!(anon, "hubcache:u:anon:/hubs/continueWatching");

        // Warmer and request path must derive identical keys from the same
        // inputs, or warmed entries would land where nothing looks them up.
        let warm = hub_cache_key("/hubs/promoted?contentdirectoryid=23", Some("admin"));
        let request = hub_cache_key("/hubs/promoted?contentdirectoryid=23", Some("admin"));
        assert_eq!(warm, request);
    }

    fn client_for_cache(host: String) -> PlexClient {
        let context = crate::models::PlexContext {
            token: Some("fakeID".to_string()),
            client_identifier: Some("replex-test".to_string()),
            ..Default::default()
        };
        PlexClient {
            http_client: reqwest_middleware::ClientBuilder::new(
                reqwest::Client::new(),
            )
            .build(),
            context,
            host,
            cache: Cache::builder().max_capacity(100).build(),
            default_headers: http::HeaderMap::new(),
        }
    }

    async fn seed(container_json: &str) -> MediaContainerWrapper<MediaContainer> {
        serde_json::from_str(container_json).unwrap()
    }

    fn mock_server_for(path: &'static str, status: u16) -> httpmock::MockServer {
        let server = httpmock::MockServer::start();
        let _ = server.mock(|when, then| {
            when.method(httpmock::Method::GET).path(path);
            if status == 200 {
                then.status(200)
                    .header("content-type", "application/json")
                    .body_from_file("tests/mock/in/hubs_sections_6.json");
            } else {
                then.status(status);
            }
        });
        server
    }

    #[tokio::test]
    async fn refresh_replaces_payload_and_tracks_age() {
        // HUB_FETCHED_AT lazily extracts Config, which needs a host set.
        std::env::set_var("REPLEX_HOST", "http://localhost:32400");
        let server = mock_server_for("/hubs/test-refresh-ok", 200);
        let client = client_for_cache(format!("http://{}", server.address()));

        let cache_key = "hubcache:test-refresh-ok";
        client
            .cache
            .insert(
                cache_key.to_string(),
                seed(r#"{"MediaContainer":{"size":0}}"#).await,
            )
            .await;
        track_fetched(cache_key.to_string()).await;
        assert!(!is_stale(cache_key, Duration::from_secs(300)).await);

        // Simulate an event-driven invalidation, then refresh.
        mark_all_hubs_stale();
        assert!(is_stale(cache_key, Duration::from_secs(300)).await);

        refresh_hub_entry(&client, "/hubs/test-refresh-ok", cache_key).await;

        let mut updated = client.cache.get(cache_key).await.unwrap();
        assert!(
            !updated.media_container.children().is_empty(),
            "refresh must swap in the fetched payload"
        );
        assert!(
            !is_stale(cache_key, Duration::from_secs(300)).await,
            "successful refresh must reset the age record"
        );
    }

    #[tokio::test]
    async fn refresh_failure_keeps_previous_payload() {
        std::env::set_var("REPLEX_HOST", "http://localhost:32400");
        let server = mock_server_for("/hubs/test-refresh-fail", 500);
        let client = client_for_cache(format!("http://{}", server.address()));

        let cache_key = "hubcache:test-refresh-fail";
        client
            .cache
            .insert(
                cache_key.to_string(),
                seed(r#"{"MediaContainer":{"size":7}}"#).await,
            )
            .await;

        refresh_hub_entry(&client, "/hubs/test-refresh-fail", cache_key).await;

        let mut kept = client.cache.get(cache_key).await.unwrap();
        assert!(
            kept.media_container.children().is_empty(),
            "failed refresh must keep the previous payload"
        );
    }

    #[test]
    fn staleness_semantics() {
        let now = Instant::now();
        assert!(is_older_than(None, Duration::from_secs(300), now));
        assert!(!is_older_than(Some(now), Duration::from_secs(300), now));
        assert!(!is_older_than(
            Some(now - Duration::from_secs(299)),
            Duration::from_secs(300),
            now
        ));
        assert!(is_older_than(
            Some(now - Duration::from_secs(301)),
            Duration::from_secs(300),
            now
        ));
    }

    #[test]
    fn invalidation_paths() {
        assert!(is_playback_invalidation("/:/scrobble", None));
        assert!(is_playback_invalidation("/:/unscrobble", None));
        // Playing/paused/buffering pings must not invalidate.
        assert!(!is_playback_invalidation(
            "/:/timeline",
            Some("state=playing&ratingKey=1")
        ));
        assert!(!is_playback_invalidation(
            "/:/timeline",
            Some("state=paused&ratingKey=1")
        ));
        assert!(!is_playback_invalidation("/:/timeline", None));
        // Stop does.
        assert!(is_playback_invalidation(
            "/:/timeline",
            Some("ratingKey=5&state=stopped&key=/library/metadata/5")
        ));
        // Unrelated paths never do.
        assert!(!is_playback_invalidation("/library/metadata/123", None));
    }
}
