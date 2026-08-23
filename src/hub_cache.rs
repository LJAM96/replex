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

    for path in warm_paths(&host, &token).await {
        let fetch_path = if path.contains('?') {
            format!("{}&includeGuids=1&count=50", path)
        } else {
            format!("{}?includeGuids=1&count=50", path)
        };
        // Compute the cache key with the exact same logic client requests
        // go through, so warmer entries land where lookups land.
        let cache_key = {
            let mut probe = salvo::http::Request::default();
            let uri: hyper::Uri = fetch_path.parse().expect("valid warm path");
            probe.set_uri(uri);
            format!("hubcache:{}", crate::routes::cache_key_for_request(&probe))
        };
        if !claim_refresh(&cache_key) {
            continue;
        }
        match fetch_hubs_payload(&client, &fetch_path).await {
            Ok(payload) => {
                client.cache.insert(cache_key.clone(), payload).await;
                track_fetched(cache_key.clone()).await;
                tracing::debug!(path = %fetch_path, "warmed hub payload");
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %fetch_path, "warm fetch failed");
            }
        }
        let mut inflight = REFRESH_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        inflight.remove(&cache_key);
    }
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
