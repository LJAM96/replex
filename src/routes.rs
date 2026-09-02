use crate::auth::{
    request_context, security_state, RequestContextMiddleware,
    SecurityContextMiddleware, SecurityContextState,
};
use crate::config::Config;
use crate::logging::*;
use crate::models::*;
use crate::plex_client::*;
use crate::resolution_policy::{
    best_allowed_media, media_allowed, resolve_policy,
};
use crate::state::{self, AppState, AppStateMiddleware};
use crate::timeout::*;
use crate::transform::*;
use crate::url::*;
use crate::utils::*;
use crate::webhooks;
use http;
use itertools::Itertools;
use once_cell::sync::Lazy;
use salvo::compression::Compression;
use salvo::cors::{AllowOrigin, Cors};
use salvo::http::header;
use salvo::http::header::CONTENT_TYPE;
use salvo::http::HeaderValue;
use salvo::http::Method;
use salvo::http::{Request, Response, StatusCode};
use salvo::prelude::*;
use salvo::routing::PathFilter;
// ResponseExt::take_bytes is used to buffer upstream bodies for the library
// disk cache; the salvo "test" feature is unconditionally enabled in
// Cargo.toml (utils::from_salvo_response relies on the same helper).
use salvo::test::ResponseExt;
use std::sync::Arc;
use tokio::time::Duration;
use url::Url;

const SKIP_OPTIONAL_TRANSFORMS_KEY: &str = "replex.skip_optional_transforms";

fn skip_optional_transforms(depot: &Depot) -> bool {
    depot
        .get::<bool>(SKIP_OPTIONAL_TRANSFORMS_KEY)
        .copied()
        .unwrap_or(false)
}

fn plex_client_from_depot(
    context: &PlexContext,
    depot: &Depot,
) -> anyhow::Result<PlexClient> {
    let state = state::from_depot(depot)?;
    let request = request_context(depot).ok();
    let effective_context = request
        .as_ref()
        .map(|request| &request.plex)
        .unwrap_or(context);
    let mut client =
        PlexClient::from_context_with_state(effective_context, &state)?;
    if let Some(request) = request {
        client.host = request.upstream_host.clone();
    }
    match security_state(depot) {
        Some(SecurityContextState::Resolved(security)) => {
            client.security_context = Some(security);
        }
        Some(SecurityContextState::Unavailable { fail_closed, .. }) => {
            client.security_unavailable_fail_closed = Some(fail_closed);
        }
        None => {}
    }
    Ok(client)
}

fn app_config(depot: &Depot) -> anyhow::Result<Arc<Config>> {
    Ok(state::from_depot(depot)?.config.clone())
}

/// Default dynamic responses to `Cache-Control: no-cache`.
///
/// Without an explicit header, browser service workers (Plex Web registers
/// one) heuristically cache API responses indefinitely, serving stale hub
/// data with dangling image references. `/web/*` assets set their own
/// long-lived policy and are left untouched.
#[handler]
async fn api_cache_control(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    ctrl.call_next(req, depot, res).await;
    let is_web_asset = req.uri().path().starts_with("/web/");
    if !is_web_asset && res.headers().get(header::CACHE_CONTROL).is_none() {
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-cache"),
        );
    }
}

pub fn route() -> Router {
    let config: Config = Config::figment()
        .extract()
        .expect("invalid Replex configuration");
    let state = Arc::new(
        AppState::new(config).expect("invalid Replex application state"),
    );
    route_with_state(state)
}

pub fn route_with_state(state: Arc<AppState>) -> Router {
    let config = &state.config;

    // cant use colon in paths. So we do it with an regex
    let guid = regex::Regex::new(":").unwrap();
    PathFilter::register_wisp_regex("colon", guid);

    // Restricted CORS: only the listed origins may read responses, and only
    // the Plex request headers/methods are permitted. Avoids the previous
    // fully permissive (wildcard) policy that let any website drive the
    // proxy on a victim's behalf. Origins are configurable via
    // REPLEX_CORS_ALLOWED_ORIGINS (comma separated); Plex Web hosts are the
    // sensible default.
    let cors = {
        let allowed_origins: Vec<HeaderValue> =
            std::env::var("REPLEX_CORS_ALLOWED_ORIGINS")
                .ok()
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .filter_map(|s| HeaderValue::from_str(&s).ok())
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| {
                    ["https://app.plex.tv", "https://plex.tv"]
                        .iter()
                        .map(|s| HeaderValue::from_static(s))
                        .collect()
                });
        Cors::new()
            .allow_origin(AllowOrigin::list(allowed_origins))
            .allow_methods(vec![
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::HEAD,
                Method::OPTIONS,
            ])
            .allow_headers(vec![
                header::ACCEPT,
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                header::ORIGIN,
                header::RANGE,
                header::HeaderName::from_static("x-plex-token"),
                header::HeaderName::from_static("x-plex-client-identifier"),
                header::HeaderName::from_static("x-plex-device-name"),
                header::HeaderName::from_static("x-plex-platform"),
                header::HeaderName::from_static("x-plex-product"),
                header::HeaderName::from_static("x-plex-version"),
                header::HeaderName::from_static("x-plex-device"),
                header::HeaderName::from_static("x-plex-session-id"),
                header::HeaderName::from_static("x-plex-playback-session-id"),
                header::HeaderName::from_static("x-plex-target-client-id"),
            ])
            .allow_credentials(true)
            .into_handler()
    };
    let mut router = Router::with_hoop(AppStateMiddleware::new(state.clone()))
        .hoop(RequestContextMiddleware)
        .hoop(SecurityContextMiddleware)
        .hoop(cors)
        .hoop(Logger::new())
        .hoop(api_cache_control)
        .hoop(should_skip)
        .hoop(Timeout::new(Duration::from_secs(60 * 200)))
        .hoop(Compression::new().enable_gzip(CompressionLevel::Fastest));
    // .hoop(affix::insert("script_engine", Arc::new(script_engine)));

    // Register the policy-gated stream handler whenever the policy is enabled
    // (so restricted accounts are proxied/enforced) OR when direct redirects
    // are requested (legacy behaviour for unlimited accounts). When neither
    // applies the generic proxy path serves streams with no policy check.
    if config.redirect_streams || config.resolution_policy_enabled {
        router = router
            .push(
                Router::with_path(
                    "/video/<colon:colon>/transcode/universal/session/<**rest>",
                )
                .goal(protected_redirect_stream),
            )
            .push(
                Router::with_path(
                    "/library/parts/<itemid>/<partid>/file.<extension>",
                )
                .goal(protected_redirect_stream),
            );
    }

    // TODO: We could just make a gobal middleware that checks every request for the includeRelated.
    // Not sure of the performance impact tho
    if config.disable_related {
        router = router
            .push(
                Router::new()
                    .path("/library/metadata/<id>/related")
                    .hoop(Timeout::new(Duration::from_secs(5)))
                    .goal(proxy_request),
            )
            .push(
                Router::with_path("/playQueues")
                    .hoop(disable_related_query)
                    .goal(proxy_request),
            );
    }

    let mut decision_router = Router::new()
        .path("/video/<colon:colon>/transcode/universal/decision")
        .goal(proxy_request);

    let mut start_router = Router::new()
        .path("/video/<colon:colon>/transcode/universal/start<**rest>")
        .goal(proxy_request);

    let mut subtitles_router = Router::new()
        .path("/video/<colon:colon>/transcode/universal/subtitles")
        .goal(proxy_request);

    // Per-user resolution policy must run before any other version
    // selection logic so every downstream decision sees an allowed list.
    if config.resolution_policy_enabled {
        decision_router = decision_router.hoop(enforce_resolution_policy);
        start_router = start_router.hoop(enforce_resolution_policy);
        subtitles_router = subtitles_router.hoop(enforce_resolution_policy);
    }

    // should go before force_maximum_quality and video_transcode_fallback
    if config.auto_select_version {
        decision_router = decision_router.hoop(auto_select_version);
        start_router = start_router.hoop(auto_select_version);
        subtitles_router = subtitles_router.hoop(auto_select_version);
    }

    if config.force_maximum_quality || config.disable_transcode {
        decision_router = decision_router.hoop(force_maximum_quality);
        start_router = start_router.hoop(force_maximum_quality);
        subtitles_router = subtitles_router.hoop(force_maximum_quality);
    }

    if config.video_transcode_fallback_for.is_some() {
        decision_router = decision_router.hoop(video_transcode_fallback);
        //subtitles_router = subtitles_router.hoop(video_transcode_fallback);
    }

    decision_router = decision_router.hoop(direct_stream_fallback);

    router = router
        .push(decision_router)
        .push(start_router)
        .push(subtitles_router);

    // Per-user resolution policy: filter prohibited media versions from
    // metadata responses. Registered before everything else so they win over
    // the generic proxy paths below. When the feature flag is off these
    // handlers leave responses untouched.
    if config.resolution_policy_enabled {
        router = router
            .push(
                Router::new()
                    .path("/library/metadata/<**rest>")
                    .hoop(proxy_for_transform)
                    .get(transform_policy_response)
                    .post(transform_policy_response),
            )
            .push(
                Router::new()
                    .path("/library/sections/<**rest>")
                    .hoop(library_cache_lookup)
                    .hoop(proxy_for_transform)
                    .hoop(library_cache_store)
                    .get(transform_policy_response),
            )
            .push(
                Router::new()
                    .path("/playQueues")
                    .hoop(proxy_for_transform)
                    .get(transform_policy_response)
                    .post(transform_policy_response)
                    .put(transform_policy_response),
            );
    }

    if config.disable_continue_watching {
        router = router.push(
            Router::new()
                .path(PLEX_CONTINUE_WATCHING)
                .get(empty_handler),
        );
    }

    if config.ntf_watchlist_force {
        router = router.push(
            Router::new()
                .hoop(ntf_watchlist_force)
                //.get(ping)
                //.hoop(debug)
                .goal(proxy_request)
                .path("/media/providers"),
        );
    }

    // Continue Watching is fetched by clients from its own endpoint; route
    // it through the hub transform chain so hero styling (REPLEX_HERO_ROWS)
    // and other hub transforms apply to it too.
    router = router.push(
        Router::new()
            .path(PLEX_CONTINUE_WATCHING)
            .hoop(transform_req_include_guids)
            .hoop(transform_req_android)
            .get(cached_hubs_response),
    );

    router = router
        .push(
            Router::new()
                .path(PLEX_HUBS_PROMOTED)
                .hoop(transform_req_content_directory)
                .hoop(transform_req_include_guids)
                .hoop(transform_req_android)
                .get(cached_hubs_response),
        )
        .push(
            Router::new()
                .path("/hubs/home")
                .hoop(transform_req_include_guids)
                .hoop(transform_req_android)
                .get(cached_hubs_response),
        )
        .push(
            Router::new()
                .path("/replex/image/hero/<type>/<uuid>")
                .get(hero_image),
        )
        .push(
            Router::new()
                .path("/web/<**rest>")
                .get(crate::web_assets::serve_web_asset),
        )
        .push(
            Router::new()
                .path(format!("{}/<id>", PLEX_HUBS_SECTIONS))
                .hoop(transform_req_include_guids)
                .hoop(transform_req_android)
                .get(cached_hubs_response),
        )
        .push(Router::new().path("/replex/webhooks").post(webhook_plex))
        .push(Router::new().path("/health/live").get(liveness))
        .push(
            Router::new()
                .path("/replex/admin/health/ready")
                .get(readiness),
        )
        .push(Router::new().path("/replex/admin/metrics").get(metrics))
        .push(Router::new().path("/replex/admin/cache").get(cache_status))
        .push(
            Router::new()
                .path("/replex/admin/cache/<class>")
                .delete(cache_purge),
        )
        .push(
            Router::new()
                .path("/replex/admin/policy/reload")
                .post(policy_reload),
        )
        .push(
            Router::new()
                .path("/replex/admin/playback/explain")
                .get(explain_playback),
        )
        .push(Router::new().path("/ping").get(ping))
        .push(
            Router::new()
                .path("/replex/<style>/library/collections/<ids>/children")
                .get(get_collections_children),
        )
        .push(
            Router::new()
                .path("/replex/<style>/<**rest>")
                .get(default_transform),
        )
        .push(
            Router::with_path("/photo/<colon:colon>/transcode")
                .hoop(photo_cache_hoop)
                .hoop(fix_photo_transcode_request)
                .hoop(resolve_local_media_path)
                .goal(proxy_request),
        )
        .push(Router::with_path("<**rest>").goal(proxy_request));

    // Development-only debugging proxy. Never present in release builds.
    #[cfg(debug_assertions)]
    {
        router = router.push(
            Router::new()
                .path("/replex/test_proxy/<**rest>")
                .goal(test_proxy_request),
        );
    }

    router
}

/// Shared in-memory cache for transcoded poster/art images.
///
/// Every /photo/:/transcode hit is a fresh PMS transcode (~0.4-0.7s each);
/// a home wall of posters therefore costs tens of seconds of serialized
/// upstream work on first view. Plex artwork is authenticated library data,
/// so entries are canonicalised by image request and scoped by token hash.
pub(crate) static PHOTO_CACHE: Lazy<moka::future::Cache<String, CachedImage>> =
    Lazy::new(|| {
        moka::future::Cache::builder()
            .max_capacity(20_000)
            .time_to_idle(std::time::Duration::from_secs(60 * 60 * 24))
            .build()
    });

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CachedImage {
    pub(crate) content_type: Option<String>,
    pub(crate) cache_control: Option<String>,
    pub(crate) body: Vec<u8>,
}

fn photo_cache_key(req: &Request) -> String {
    let raw = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    photo_cache_key_for(&raw, photo_request_token(req).as_deref())
}

/// Photo transcode requests normally carry authentication in the standard
/// header or top-level query, but some Plex clients place it only inside the
/// nested `url=` value. Resolve all supported placements before selecting the
/// artwork cache namespace so a nested-token request can never fall into the
/// shared anonymous scope.
fn photo_request_token(req: &Request) -> Option<String> {
    request_token(req).or_else(|| {
        req.queries()
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("url"))
            .and_then(|(_, value)| extract_inner_token(value))
    })
}

fn extract_inner_token(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let marker = "x-plex-token=";
    let start = lower.find(marker)? + marker.len();
    let remainder = &value[start..];
    let end = remainder.find(['&', '#']).unwrap_or(remainder.len());
    let token = &remainder[..end];
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Canonical image identity for a photo request path+query. Strips the per-user
/// token from BOTH the top-level query and inside the nested `url=`
/// parameter (Plex Web appends its own token there). Account identity is added
/// separately by `photo_cache_key_for`, so token placement cannot fragment the
/// cache while two accounts can never share an authenticated artwork entry.
pub(crate) fn canonical_photo_key(raw: &str) -> String {
    match Url::parse(&format!("http://x{}", raw)) {
        Ok(mut url) => {
            let pairs: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| !k.eq_ignore_ascii_case("X-Plex-Token"))
                .map(|(k, v)| {
                    let k = k.to_string();
                    if k == "url" {
                        (k.clone(), strip_inner_token(&v))
                    } else {
                        (k.clone(), v.to_string())
                    }
                })
                .collect();
            {
                let mut ser = url.query_pairs_mut();
                ser.clear();
                for (k, v) in &pairs {
                    ser.append_pair(k, v);
                }
            }
            format!("photo:{}", url)
        }
        Err(_) => format!("photo:{}", raw),
    }
}

fn strip_inner_token(v: &str) -> String {
    let Some((base, query)) = v.split_once('?') else {
        return v.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            !pair
                .split_once('=')
                .map(|(key, _)| key.eq_ignore_ascii_case("x-plex-token"))
                .unwrap_or(false)
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// Short hash of a token used as the user-scope component of library cache
/// keys. Raw tokens never appear in cache keys.
/// Query parameters that carry no payload identity for library requests:
/// - `X-Plex-*` client/session metadata — everything except the container
///   paging params, which DO define the payload;
/// - field-trimming / cosmetic shaping params (`excludeFields`,
///   `includeFields`, `includeGeolocation`). Cache misses fetch without
///   these (see `normalize_library_fetch`), so every stored raw payload is
///   a full-field superset and dropping them from keys can never serve a
///   client a trimmed payload it did not ask for.
///
/// Payload-defining filters (`type`, `sort`, `genre`, `unwatched`, ...) and
/// content-shaping flags like `excludeAllLeaves` are preserved: they change
/// which items come back and must stay part of the key.
fn is_library_key_noise(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "x-plex-container-start" || lower == "x-plex-container-size" {
        return false;
    }
    lower.starts_with("x-plex-")
        || matches!(
            lower.as_str(),
            "includefields" | "excludefields" | "includegeolocation"
        )
}

/// Field-shaping params stripped from the upstream fetch on cache misses.
/// Deliberately narrower than `is_library_key_noise`: the fetch must
/// preserve the client's token and everything Plex itself acts on; only
/// the response-field shaping is removed so stored payloads are supersets,
/// matching what the warmer fetches.
fn is_library_fetch_shaping(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "includefields" | "excludefields" | "includegeolocation"
    )
}

/// Canonical path+query for library cache keys: client-shaping noise is
/// stripped (`is_library_key_noise`), remaining keys are lower-cased and
/// the pairs sorted, so the same logical request — whatever client shape
/// it arrived in — maps to exactly one entry. Tokens never appear in keys.
pub(crate) fn canonical_library_path(raw_path_and_query: &str) -> String {
    match Url::parse(&format!("http://x{}", raw_path_and_query)) {
        Ok(mut url) => {
            let mut pairs: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| !is_library_key_noise(k))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                .collect();
            pairs.sort();
            {
                let mut ser = url.query_pairs_mut();
                ser.clear();
                for (k, v) in &pairs {
                    ser.append_pair(k, v);
                }
            }
            format!("library:{}", url)
        }
        Err(_) => format!("library:{}", raw_path_and_query),
    }
}

/// Strip field-shaping params from a `/library/sections` request before the
/// upstream fetch on cache misses, so the raw payload persisted for the
/// account's scope is a full-field superset any later request in that scope
/// can consume (the warmer's fetches are already superset-shaped). Auth
/// token and paging params are always preserved.
fn normalize_library_fetch(req: &mut Request) {
    let raw = match req.uri().path_and_query() {
        Some(p) => p.to_string(),
        None => return,
    };
    let mut url = match Url::parse(&format!("http://x{}", raw)) {
        Ok(u) => u,
        Err(_) => return,
    };
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !is_library_fetch_shaping(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    {
        let mut ser = url.query_pairs_mut();
        ser.clear();
        for (k, v) in &kept {
            ser.append_pair(k, v);
        }
    }
    let new = match url.query() {
        Some(q) if !q.is_empty() => format!("{}?{}", url.path(), q),
        _ => url.path().to_string(),
    };
    if let Ok(uri) = new.parse::<hyper::Uri>() {
        req.set_uri(uri);
    }
}

/// Build the canonical library disk-cache key for a request.
///
/// The key is user-scoped (hash of the requesting token): raw
/// `/library/sections` payloads embed per-account watch state (viewCount,
/// lastViewedAt, userRating), so one account must never be served another
/// account's stored body.
///
/// The body stored under this key is the RAW upstream payload — per-user
/// transforms (resolution policy, collection visibility) always re-run on
/// retrieval, so a cached entry can never act as a stale authorisation
/// decision for the account that owns it. Client-shaping noise is
/// canonicalized away (`canonical_library_path`), so the library warmer
/// — which builds its keys through this same function — warms entries that
/// real client requests actually consume.
pub(crate) fn library_cache_key_for(
    raw_path_and_query: &str,
    token: Option<&str>,
) -> String {
    let scope = token
        .map(|token| crate::account_scope::token_scope(Some(token)))
        .unwrap_or_else(|| "anon".to_string());
    format!(
        "library:u:{}:{}",
        scope,
        canonical_library_path(raw_path_and_query)
    )
}

/// Extract the Plex token from a request: header first, then query param,
/// since Plex clients use both interchangeably.
fn request_token(req: &Request) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get("X-Plex-Token")
        .and_then(|v| v.to_str().ok())
    {
        return Some(v.to_string());
    }
    req.queries()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-plex-token"))
        .map(|(_, v)| v.clone())
}

fn library_cache_key(req: &Request) -> String {
    let raw = req
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    library_cache_key_for(&raw, request_token(req).as_deref())
}

/// Disk-cache lookup for `/library/sections` responses.
///
/// The persisted body is the RAW upstream payload, never an already
/// transformed one: on a hit the requesting account's CURRENT policy
/// transforms (resolution restrictions, hidden collections) re-run before
/// serving, so a cache entry can never carry an authorisation decision
/// made under an old policy or for a different account between requests.
///
/// On a miss the upstream fetch is normalized first
/// (`normalize_library_fetch`) so the raw payload stored for the account's
/// scope is a full-field superset that any future client shape can consume.
#[handler]
async fn library_cache_lookup(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) -> Result<(), anyhow::Error> {
    if !req.uri().path().starts_with("/library/sections/") {
        ctrl.call_next(req, depot, res).await;
        return Ok(());
    }
    let key = library_cache_key(req);
    if let Some(data) = crate::disk_cache::get(&key).await {
        if let Ok(state) = state::from_depot(depot) {
            state.metrics.cache_hit();
        }
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-cache"),
        );
        res.headers_mut().insert(
            CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        *res.body_mut() =
            salvo::http::body::ResBody::Once(bytes::Bytes::from(data));
        match apply_policy_transforms(req, depot, res).await {
            Ok(()) => {
                ctrl.skip_rest();
                return Ok(());
            }
            Err(error) => {
                // Corrupt/unparseable cached body: drop it and fall through
                // to a live upstream fetch instead of failing the request.
                tracing::warn!(
                    error = ?error,
                    key = %key,
                    "cached library payload unparseable, refetching"
                );
                crate::disk_cache::remove(&key).await;
            }
        }
    }
    if let Ok(state) = state::from_depot(depot) {
        state.metrics.cache_miss();
    }
    // Miss (or corrupt cached entry that was just evicted): normalize the
    // fetch so every stored raw payload is a full-field superset that any
    // future request in this account's scope can consume.
    normalize_library_fetch(req);
    ctrl.call_next(req, depot, res).await;
    Ok(())
}

/// Persist the RAW upstream `/library/sections` payload after the proxy
/// fetch and BEFORE the policy transform runs in the goal handler. Storing
/// pre-transform bodies is what keeps the disk cache from ever becoming an
/// authorisation artifact: the stored bytes contain no decision made under
/// any policy.
#[handler]
async fn library_cache_store(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    if !req.uri().path().starts_with("/library/sections/") {
        ctrl.call_next(req, depot, res).await;
        return;
    }
    if res.status_code.unwrap_or(StatusCode::OK).is_success() {
        // The upstream body may still be streaming at this point; buffer it
        // so we can persist the raw payload and hand a complete body to the
        // transform goal below.
        let bytes = match res.take_bytes(None).await {
            Ok(b) => b,
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    "could not buffer upstream library payload for disk cache"
                );
                ctrl.call_next(req, depot, res).await;
                return;
            }
        };
        if !bytes.is_empty() {
            let key = library_cache_key(req);
            let _ = crate::disk_cache::put(&key, &bytes).await;
            *res.body_mut() = salvo::http::body::ResBody::Once(bytes);
        }
    }
    ctrl.call_next(req, depot, res).await;
}

/// Cache-aside layer for /photo/:/transcode responses.
#[handler]
async fn photo_cache_hoop(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) -> Result<(), anyhow::Error> {
    if skip_optional_transforms(depot) {
        do_proxy_request(req, res, depot, ctrl).await;
        ctrl.skip_rest();
        return Ok(());
    }

    let key = photo_cache_key(req);
    if let Some(record) = crate::disk_cache::get_full(&key).await {
        if let Ok(state) = state::from_depot(depot) {
            state.metrics.cache_hit();
        }
        // A persisted photo keeps its original content type (WebP, PNG, ...).
        // Fall back to image/jpeg for legacy entries that stored none.
        let ct = record
            .content_type
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "image/jpeg".to_string());
        if let Ok(v) = header::HeaderValue::from_str(&ct) {
            res.headers_mut().insert(CONTENT_TYPE, v);
        }
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("public, max-age=259200"),
        );
        *res.body_mut() =
            salvo::http::body::ResBody::Once(bytes::Bytes::from(record.body));
        return Ok(());
    }
    if let Some(img) = PHOTO_CACHE.get(&key).await {
        if let Ok(state) = state::from_depot(depot) {
            state.metrics.cache_hit();
        }
        if let Some(ct) = img.content_type.as_deref() {
            if let Ok(v) = header::HeaderValue::from_str(ct) {
                res.headers_mut().insert(CONTENT_TYPE, v);
            }
        }
        if let Some(cc) = img.cache_control.as_deref() {
            if let Ok(v) = header::HeaderValue::from_str(cc) {
                res.headers_mut().insert(header::CACHE_CONTROL, v);
            }
        }
        *res.body_mut() =
            salvo::http::body::ResBody::Once(bytes::Bytes::from(img.body));
        return Ok(());
    }

    if let Ok(state) = state::from_depot(depot) {
        state.metrics.cache_miss();
    }

    do_proxy_request(req, res, depot, ctrl).await;

    // Only cache successful image payloads.
    let status_ok = res.status_code.unwrap_or(StatusCode::OK).is_success();
    let is_image = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("image/"))
        .unwrap_or(false);
    if !(status_ok && is_image) {
        return Ok(());
    }

    let taken =
        std::mem::replace(res.body_mut(), salvo::http::body::ResBody::None);
    if let salvo::http::body::ResBody::Once(body) = taken {
        let should_cache = body.len() <= 4 * 1024 * 1024
            && res
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.starts_with("image/"))
                .unwrap_or(false)
            && res.status_code.unwrap_or(StatusCode::OK).is_success();
        if should_cache {
            let entry = CachedImage {
                content_type: res
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string()),
                cache_control: res
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string()),
                body: body.to_vec(),
            };
            let n = entry.body.len();
            PHOTO_CACHE.insert(key.clone(), entry).await;
            let content_type = res
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let _ = crate::disk_cache::put_full(
                &key,
                &body,
                content_type.as_deref(),
            )
            .await;
            tracing::debug!(bytes = n, "cached photo response");
        }
        *res.body_mut() = salvo::http::body::ResBody::Once(body);
    }
    Ok(())
}

/// Raw proxy pass-through shared by the catch-all handler and the
/// photo cache hoop.
async fn do_proxy_request(
    req: &mut Request,
    res: &mut Response,
    depot: &mut Depot,
    ctrl: &mut FlowCtrl,
) {
    let state = match state::from_depot(depot) {
        Ok(state) => state,
        Err(error) => {
            tracing::error!(error = %error, "proxy request missing shared state");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            ctrl.skip_rest();
            return;
        }
    };
    let proxy = default_proxy(&state);
    let started = std::time::Instant::now();
    proxy.handle(req, depot, res, ctrl).await;
    state.metrics.observe_upstream(
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        res.status_code.unwrap_or(StatusCode::OK).is_success(),
    );
}

#[handler]
async fn proxy_request(
    req: &mut Request,
    res: &mut Response,
    depot: &mut Depot,
    ctrl: &mut FlowCtrl,
) {
    // Playback changes flowing through the proxy (scrobbles, playback
    // stopping) affect what hubs display. Mark all hub payloads stale so
    // the next request serves instantly while refreshing in the background.
    if crate::hub_cache::is_playback_invalidation(
        req.uri().path(),
        req.uri().query(),
    ) {
        crate::hub_cache::mark_all_hubs_stale();
        tracing::debug!(
            path = %req.uri().path(),
            "playback change observed, hub cache marked stale"
        );
    }
    do_proxy_request(req, res, depot, ctrl).await;
}

/// Development-only debugging proxy. Deliberately excluded from release
/// builds: a production service must not ship an externally reachable path
/// that forwards requests to an arbitrary third-party endpoint.
#[cfg(debug_assertions)]
#[handler]
async fn test_proxy_request(
    req: &mut Request,
    res: &mut Response,
    depot: &mut Depot,
    ctrl: &mut FlowCtrl,
) {
    let proxy = test_proxy("https://webhook.site".to_string());
    proxy.handle(req, depot, res, ctrl).await;
}

#[handler]
async fn proxy_for_transform(
    req: &mut Request,
    res: &mut Response,
    depot: &mut Depot,
    ctrl: &mut FlowCtrl,
) -> Result<(), anyhow::Error> {
    let state = state::from_depot(depot)?;
    let proxy = default_proxy(&state);
    let headers_ori = req.headers().clone();
    req.headers_mut().insert(
        http::header::ACCEPT,
        header::HeaderValue::from_static("application/json"),
    );
    proxy.handle(req, depot, res, ctrl).await;
    *req.headers_mut() = headers_ori;
    Ok(())
}

// Plexamp and Live TV historically bypassed every Replex handler here by
// proxying immediately and calling skip_rest(). Because this hoop is global,
// that also bypassed mandatory resolution-policy and direct-part enforcement.
// Keep the compatibility classification, but only store it as request-scoped
// state. Security handlers ignore this flag; optional transforms may opt out.
#[handler]
async fn should_skip(_req: &mut Request, depot: &mut Depot) {
    let Ok(request) = request_context(depot) else {
        return;
    };
    let context = &request.plex;

    let is_livetv = context
        .path
        .as_deref()
        .is_some_and(|v| v.contains("livetv"));

    let is_plexamp = context
        .product
        .as_deref()
        .is_some_and(|v| v.to_lowercase().contains("plexamp"));

    let _ = depot.insert(SKIP_OPTIONAL_TRANSFORMS_KEY, is_livetv || is_plexamp);
}

/// Build the account-scoped artwork cache key used by both live requests and
/// the background warmer. Raw Plex tokens never appear in cache keys.
pub(crate) fn photo_cache_key_for(raw: &str, token: Option<&str>) -> String {
    let scope = token
        .map(|token| crate::account_scope::token_scope(Some(token)))
        .unwrap_or_else(|| "anon".to_string());
    format!("photo:u:{scope}:{}", canonical_photo_key(raw))
}

async fn perform_stream_redirect(
    req: &mut Request,
    depot: &Depot,
    res: &mut Response,
) -> anyhow::Result<()> {
    let config = app_config(depot)?;
    let request = request_context(depot)?;
    let base = config
        .redirect_streams_host
        .as_deref()
        .unwrap_or(request.upstream_host.as_str());
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(req.uri().path());
    let redirect_url = format!("{base}{path_and_query}");
    let mime = mime_guess::from_path(req.uri().path()).first_or_octet_stream();
    if let Ok(content_type) = mime.as_ref().parse() {
        res.headers_mut().insert(CONTENT_TYPE, content_type);
    }
    res.render(Redirect::temporary(redirect_url));
    if let Ok(state) = state::from_depot(depot) {
        state.metrics.redirect();
    }
    Ok(())
}

/// Proxy a stream through Replex instead of redirecting the client to the
/// Plex origin. Used for restricted accounts so the byte path stays behind the
/// policy check and the client never learns the Plex origin URL — required for
/// resolution limits to be enforceable. The upstream fetch reuses the
/// requesting account's own token, so per-account watch state is preserved.
async fn proxy_stream_through_replex(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    do_proxy_request(req, res, depot, ctrl).await;
    ctrl.skip_rest();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamDelivery {
    Proxy,
    Redirect,
}

fn stream_delivery(
    restricted: bool,
    redirects_enabled: bool,
) -> StreamDelivery {
    if restricted || !redirects_enabled {
        StreamDelivery::Proxy
    } else {
        StreamDelivery::Redirect
    }
}

async fn deliver_stream(
    delivery: StreamDelivery,
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) -> anyhow::Result<()> {
    let request_id = request_context(depot)
        .ok()
        .map(|context| context.request_id.clone())
        .unwrap_or_else(|| "unknown".to_string());
    tracing::info!(
        request_id = %request_id,
        path = %req.uri().path(),
        stream_transport = ?delivery,
        "Stream transport selected"
    );
    match delivery {
        StreamDelivery::Proxy => {
            proxy_stream_through_replex(req, depot, res, ctrl).await;
            Ok(())
        }
        StreamDelivery::Redirect => {
            perform_stream_redirect(req, depot, res).await
        }
    }
}

/// Stream gating by the resolution policy.
///
/// Order matters: authenticate, apply policy, and only then serve the bytes.
/// - Policy disabled: obey the configured redirect mode.
/// - Unrestricted account: obey the configured redirect mode.
/// - Restricted account: the bytes are proxied THROUGH Replex, never handed
///   the Plex origin URL, so the limit stays enforceable. `/library/parts`
///   requests are checked against cached resolution and bitrate classification
///   facts; prohibited parts get
///   403 and unknown parts are blocked too (a restricted account may only
///   stream parts Replex has seen and permitted).
/// - Transcode session requests: identity must verify (fail closed), then the
///   session is proxied through Replex like any other restricted stream.
///
/// NOTE: enforcement only holds if clients cannot reach the Plex origin
/// directly. Deploy Replex as the sole path to Plex (see README).
#[handler]
async fn protected_redirect_stream(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let config = match app_config(depot) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %error, "stream request missing application configuration");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    let runtime_policy = match state::from_depot(depot) {
        Ok(state) => state.policy.snapshot().await,
        Err(error) => {
            tracing::error!(error = %error, "stream request missing policy state");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    if !runtime_policy.enabled {
        if let Err(error) = deliver_stream(
            stream_delivery(false, config.redirect_streams),
            req,
            depot,
            res,
            ctrl,
        )
        .await
        {
            tracing::error!(error = %error, "stream delivery failed");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        }
        return;
    }

    let context = match request_context(depot) {
        Ok(context) => context.plex.clone(),
        Err(error) => {
            tracing::warn!(error = %error, "stream request missing Plex context");
            res.status_code(StatusCode::BAD_REQUEST);
            return;
        }
    };
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            res.status_code(StatusCode::UNAUTHORIZED);
            return;
        }
    };

    let (identity, policy) = match security_state(depot) {
        Some(SecurityContextState::Resolved(security)) => {
            (security.identity.clone(), security.policy.clone())
        }
        Some(SecurityContextState::Unavailable {
            fail_closed,
            reason,
        }) => {
            if fail_closed {
                tracing::warn!(
                    error = %reason,
                    path = %req.uri().path(),
                    "Identity unavailable, failing closed on stream request"
                );
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                return;
            }
            tracing::warn!(
                error = %reason,
                path = %req.uri().path(),
                "Identity unavailable, failing open on stream request"
            );
            if let Err(error) = deliver_stream(
                stream_delivery(false, config.redirect_streams),
                req,
                depot,
                res,
                ctrl,
            )
            .await
            {
                tracing::error!(error = %error, "fail-open stream delivery failed");
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            }
            return;
        }
        None => match plex_client.get_current_user().await {
            Ok(identity) => {
                let policy = resolve_policy(
                    &runtime_policy.entries,
                    runtime_policy.default_limit,
                    &runtime_policy.hidden_collections,
                    &identity,
                );
                (identity, policy)
            }
            Err(error) => {
                tracing::warn!(error = %error, "Identity unavailable on stream request");
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                return;
            }
        },
    };
    let stream_restricted =
        !policy.is_unrestricted() || policy.max_bitrate.is_some();
    if !stream_restricted {
        if let Err(error) = deliver_stream(
            stream_delivery(false, config.redirect_streams),
            req,
            depot,
            res,
            ctrl,
        )
        .await
        {
            tracing::error!(error = %error, "unrestricted stream delivery failed");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        }
        return;
    }

    // Restricted account: the stream MUST stay behind Replex so the policy
    // decision is enforced and the client never learns the Plex origin URL. A
    // direct 302 to the origin would let the client bypass the limit entirely.
    if let Some(part_id) = req.param::<i64>("partid") {
        match plex_client.part_media_cache.get(&part_id).await {
            Some(classification)
                if crate::plex_client::part_classification_allowed(
                    classification,
                    &policy,
                ) =>
            {
                tracing::debug!(
                    username = %identity.username,
                    part_id = part_id,
                    "Permitted part requested; proxying through Replex"
                );
                if let Err(error) = deliver_stream(
                    stream_delivery(stream_restricted, config.redirect_streams),
                    req,
                    depot,
                    res,
                    ctrl,
                )
                .await
                {
                    tracing::error!(error = %error, "restricted part delivery failed");
                    res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
            Some(_) => {
                if let Ok(state) = state::from_depot(depot) {
                    state.metrics.policy_reject();
                }
                tracing::info!(
                    username = %identity.username,
                    part_id = part_id,
                    maximum = ?policy.limit,
                    "Blocked direct access to prohibited part"
                );
                res.status_code(StatusCode::FORBIDDEN);
            }
            None => {
                if let Ok(state) = state::from_depot(depot) {
                    state.metrics.policy_reject();
                }
                tracing::info!(
                    username = %identity.username,
                    part_id = part_id,
                    "Unknown part blocked for restricted account (enforcement)"
                );
                res.status_code(StatusCode::FORBIDDEN);
            }
        }
        return;
    }

    // Transcode session request for a restricted account: proxy through Replex
    // so the byte path is enforced too.
    if let Err(error) = deliver_stream(
        stream_delivery(stream_restricted, config.redirect_streams),
        req,
        depot,
        res,
        ctrl,
    )
    .await
    {
        tracing::error!(error = %error, "restricted transcode delivery failed");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
    }
}

// Google tv requests some weird thumbnail for hero elements. Let fix that
#[handler]
async fn fix_photo_transcode_request(
    req: &mut Request,
    _depot: &mut Depot,
    _res: &mut Response,
) {
    let context = match req.extract::<PlexContext>().await {
        Ok(context) => context,
        Err(error) => {
            tracing::debug!(error = ?error, "Ignoring malformed optional photo context");
            return;
        }
    };
    if let Some(size) = context
        .size
        .as_deref()
        .filter(|value| value.contains('-'))
        .and_then(|value| value.rsplit('-').next())
        .filter(|value| !value.is_empty())
    {
        add_query_param_salvo(req, "height".to_string(), size.to_string());
        add_query_param_salvo(req, "width".to_string(), size.to_string());
    }
}

// resolve a local media path to full url
#[handler]
async fn resolve_local_media_path(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) {
    let context = match req.extract::<PlexContext>().await {
        Ok(context) => context,
        Err(error) => {
            tracing::debug!(error = ?error, "Ignoring malformed optional media path context");
            return;
        }
    };
    let Some(raw_url) = req.query::<String>("url") else {
        return;
    };
    if raw_url.contains("/replex/image/hero") {
        let Ok(uri) = url::Url::parse(&raw_url) else {
            tracing::debug!("Ignoring malformed hero image URL");
            return;
        };
        let Some(uuid) = uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .map(|segment| segment.replace(".jpg", ""))
            .filter(|uuid| !uuid.is_empty())
        else {
            return;
        };
        //if context.token.is_none() {
        //    context.token = Some(segments.last().unwrap().to_string());
        //}

        let plex_client = match plex_client_from_depot(&context, depot) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "failed to build Plex client from request context");
                res.status_code(StatusCode::UNAUTHORIZED);
                return;
            }
        };
        if let Some(rurl) = plex_client.get_hero_art(uuid).await {
            add_query_param_salvo(req, "url".to_string(), rurl);
        }
    }
}

#[handler]
async fn disable_related_query(
    req: &mut Request,
    depot: &mut Depot,
    _res: &mut Response,
    _ctrl: &mut FlowCtrl,
) {
    if skip_optional_transforms(depot) {
        return;
    }

    add_query_param_salvo(req, "includeRelated".to_string(), "0".to_string());
}

#[handler]
async fn debug(
    req: &mut Request,
    depot: &mut Depot,
    _res: &mut Response,
    _ctrl: &mut FlowCtrl,
) {
    if let Ok(context) = request_context(depot) {
        tracing::debug!(
            request_id = %context.request_id,
            token_scope = %context.token_scope,
            path = %req.uri().path(),
            "Debug request"
        );
    }
}

#[handler]
async fn ntf_watchlist_force(
    _req: &mut Request,
    depot: &mut Depot,
    _res: &mut Response,
    _ctrl: &mut FlowCtrl,
) {
    if skip_optional_transforms(depot) {
        return;
    }

    let context = match request_context(depot) {
        Ok(context) => context.plex.clone(),
        Err(error) => {
            tracing::debug!(error = %error, "Skipping optional notification bootstrap without request context");
            return;
        }
    };
    let state = match state::from_depot(depot) {
        Ok(state) => state,
        Err(error) => {
            tracing::debug!(error = %error, "Skipping optional notification bootstrap without application state");
            return;
        }
    };
    if let (Some(token), Some(client_id)) =
        (context.token.clone(), context.client_identifier.clone())
    {
        let job_key = format!(
            "notifications:{}",
            crate::account_scope::token_scope(Some(&token))
        );
        if !state.jobs.try_start(&job_key).await {
            return;
        }
        let client = state.identity_http.clone();
        let jobs = state.jobs.clone();
        tokio::spawn(async move {
            let success = async {
                let url = "https://notifications.plex.tv/api/v1/notifications/settings";
                let json_data = r#"{"enabled": true,"libraries": [],"identifier": "tv.plex.notification.library.new"}"#;

            tracing::info!(
                username = %context.username.clone().unwrap_or_default(),
                product = %context.product.clone().unwrap_or_default(),
                device = %context.device_name.clone().unwrap_or_default(),
                "Notification bootstrap for request"
            );

            let client_base = "https://clients.plex.tv";
            let res = match client
                .get(format!("{}/api/v2/user", client_base))
                .header("Accept", "application/json")
                .header("X-Plex-Token", &token)
                .header("X-Plex-Client-Identifier", &client_id)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    tracing::debug!(error = %error, "Notification bootstrap identity request failed");
                    return false;
                }
            };

            if !res.status().is_success() {
                tracing::info!(status = %res.status(), "Notification bootstrap identity rejected");
                return false;
            }

            let user: PlexUser = match res.json().await {
                Ok(user) => user,
                Err(error) => {
                    tracing::debug!(error = %error, "Notification bootstrap identity response was invalid");
                    return false;
                }
            };
            tracing::info!(
                id = %user.id,
                uuid = %user.uuid,
                username = %user.username,
                "got user"
            );

            let response = match client
                .post(url)
                .query(&[("X-Plex-Token", token.as_str())])
                .header("Content-Type", "application/json")
                .body(json_data.to_owned())
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    tracing::debug!(error = %error, "Notification settings request failed");
                    return false;
                }
            };

            tracing::info!(
                status = %response.status(),
                "watchlist status"
            );

            let opts = vec!["tv.plex.provider.vod", "tv.plex.provider.music"];

            //let
            //return;
            let u = format!(
                "{}/api/v2/user/{}/settings/opt_outs",
                client_base, &user.uuid
            );
            let mut all_succeeded = response.status().is_success();
            for key in opts {
                let response = match client
                    .post(format!("{}?key={}&value=opt_out", u.clone(), key))
                    .header("Accept", "application/json")
                    .header("X-Plex-Token", &token)
                    .header("X-Plex-Client-Identifier", &client_id)
                    .send()
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        tracing::debug!(error = %error, key = key, "Notification opt-out request failed");
                        all_succeeded = false;
                        continue;
                    }
                };

                all_succeeded &= response.status().is_success();

                tracing::info!(
                status = %response.status(),
                "opt out status"
                );
            }
            all_succeeded
            }
            .await;
            jobs.finish(&job_key, success).await;
        });
    }
}

#[handler]
pub async fn empty_handler(
    req: &mut Request,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let content_type = get_content_type_from_headers(req.headers_mut());
    let mut container: MediaContainerWrapper<MediaContainer> =
        MediaContainerWrapper {
            content_type: content_type.clone(),
            ..Default::default()
        };
    // container.media_container.size = Some(0);
    container.media_container.identifier =
        Some("com.plexapp.plugins.library".to_string());
    res.render(container);
    return Ok(());
}

#[handler]
pub async fn webhook_plex(
    req: &mut Request,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let raw = req.form::<String>("payload").await.unwrap_or_default();
    match serde_json::from_str::<webhooks::Payload>(&raw) {
        Ok(payload) => {
            // Optional complement to proxy-side invalidation: catches changes
            // made outside the proxy. Requires the server owner's Plex Pass
            // and a publicly reachable webhook URL; harmless when unused.
            if payload.event.starts_with("media.") {
                crate::hub_cache::mark_all_hubs_stale();
                tracing::debug!(
                    event = %payload.event,
                    account = %payload.account.title,
                    "webhook marked hub cache stale"
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, "unparseable webhook payload"),
    }
    res.render(());
    Ok(())
}

#[handler]
pub async fn hero_image(
    req: &mut Request,
    res: &mut Response,
    _ctrl: &mut FlowCtrl,
    depot: &mut Depot,
) {
    let context = match req.extract::<PlexContext>().await {
        Ok(context) => context,
        Err(error) => {
            tracing::debug!(error = ?error, "Hero image request contained invalid context");
            res.status_code(StatusCode::BAD_REQUEST);
            return;
        }
    };
    let Some(_image_type) = req.param::<String>("type") else {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    };
    let Some(uuid) = req.param::<String>("uuid") else {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    };

    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            res.status_code(StatusCode::UNAUTHORIZED);
            return;
        }
    };
    let Some(url) = plex_client.get_hero_art(uuid).await else {
        res.status_code(StatusCode::NOT_FOUND);
        return;
    };
    // let uri = url.unwrap().parse::<http::Uri>().unwrap();;
    // req.set_uri(uri);
    // let proxy = proxy("https://metadata-static.plex.tv".to_string());
    // proxy.handle(req, depot, res, ctrl).await;

    // Provider coverArt URLs are not always percent-encoded (e.g. fanart.tv
    // returns `X-files (2).jpg` with a literal space). Salvo's Redirect
    // panics on an invalid URI, so sanitize before redirecting.
    let sanitized = url.replace(' ', "%20");
    if sanitized.parse::<http::Uri>().is_ok() {
        res.render(Redirect::found(sanitized));
    } else if let Ok(parsed) = url::Url::parse(&sanitized) {
        res.render(Redirect::found(parsed.to_string()));
    } else {
        tracing::warn!(url = %url, sanitized = %sanitized, "invalid hero image URL, cannot redirect");
        res.status_code(StatusCode::NOT_FOUND);
    }
}

// if directplay fails we remove it.
#[handler]
pub async fn direct_stream_fallback(
    req: &mut Request,
    _res: &mut Response,
    ctrl: &mut FlowCtrl,
    depot: &mut Depot,
) -> Result<(), anyhow::Error> {
    if skip_optional_transforms(depot) {
        return Ok(());
    }

    let _config = app_config(depot)?;
    let context = request_context(depot)?.plex.clone();
    let _plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let queries = req.queries().clone();

    let direct_play =
        queries.get("directPlay").map(String::as_str).unwrap_or("1");

    if direct_play != "1" {
        return Ok(());
    }

    let res_upstream = &mut Response::new();
    proxy_for_transform
        .handle(req, depot, res_upstream, ctrl)
        .await;

    match res_upstream.status_code.unwrap_or(StatusCode::BAD_GATEWAY) {
        http::StatusCode::OK => {
            let container: MediaContainerWrapper<MediaContainer> =
            //from_reqwest_response(upstream_res).await?;
            from_salvo_response(res_upstream).await?;

            if container.media_container.general_decision_code == Some(2000) {
                tracing::debug!(
                    "Direct play not avaiable, falling back to direct stream"
                );
                add_query_param_salvo(
                    req,
                    "directPlay".to_string(),
                    "0".to_string(),
                );
                add_query_param_salvo(
                    req,
                    "directStream".to_string(),
                    "1".to_string(),
                );
            };
            //return Ok(());
        }
        http::StatusCode::BAD_REQUEST => {
            tracing::debug!(
                "Got 400 bad request, falling back to direct stream"
            );
            add_query_param_salvo(
                req,
                "directPlay".to_string(),
                "0".to_string(),
            );
            add_query_param_salvo(
                req,
                "directStream".to_string(),
                "1".to_string(),
            );
            //return Ok(());
        }
        status => {
            tracing::error!(status = ?status, res = ?res_upstream, "Failed to get plex response");
            return Err(
                salvo::http::StatusError::internal_server_error().into()
            );
        }
    };
    //res = &mut Response::new();
    return Ok(());
}

/// Filters metadata responses through the resolution policy.
///
/// Only runs when `resolution_policy_enabled` is set (the route itself is
/// only registered then). Non-OK statuses and non-metadata payloads (images,
/// binaries) pass through untouched.
#[handler]
pub async fn transform_policy_response(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    // Only successful metadata payloads are worth parsing.
    let status = res.status_code.unwrap_or(StatusCode::OK);
    if status != StatusCode::OK {
        return Ok(());
    }
    apply_policy_transforms(req, depot, res).await
}

/// Parse the RAW upstream JSON body currently held in `res` and apply the
/// requesting account's CURRENT policy transforms over it, then render the
/// result. Shared by the live path (`transform_policy_response`) and the
/// disk-cache hit path (`library_cache_lookup`) so both always transform
/// with the current account and current configuration — a cached body is
/// never served as an authorisation decision.
async fn apply_policy_transforms(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let content_type = get_content_type_from_headers(req.headers_mut());

    let parse_result = from_salvo_response(res).await;
    let mut container: MediaContainerWrapper<MediaContainer> =
        match parse_result {
            Ok(c) => c,
            Err(error) => {
                // Binary/image/unparseable payload: body was consumed, so proxy
                // semantics require an error rather than a truncated response.
                tracing::warn!(
                    error = ?error,
                    path = %req.uri().path(),
                    "Policy transform could not parse response"
                );
                return Err(salvo::http::StatusError::bad_gateway().into());
            }
        };
    container.content_type = content_type;

    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };

    TransformBuilder::new(plex_client, context.clone())
        .with_transform(ResolutionPolicyTransform)
        .with_transform(CollectionVisibilityTransform)
        .apply_to(&mut container)
        .await;

    res.render(container);
    Ok(())
}

/// Hub responses fetched through Replex's account-scoped response cache.
///
/// The path component is canonical: only directory selection and paging
/// params define the payload shape. Account identity is added separately by
/// `hub_cache_key`, because even section discovery metadata is subject to Plex
/// library sharing and must never be reused across account tokens.
pub(crate) fn cache_key_for_request(req: &Request) -> String {
    let path = req.uri().path();
    let queries = req.queries();

    let pick = |name: &str| -> Option<&String> {
        queries
            .get(name)
            .or_else(|| queries.get(&name.to_ascii_lowercase()))
    };

    let mut parts: Vec<(&str, &String)> = vec![];
    for name in [
        "contentDirectoryID",
        "pinnedContentDirectoryID",
        "X-Plex-Container-Start",
        "X-Plex-Container-Size",
    ] {
        if let Some(v) = pick(name) {
            parts.push((name, v));
        }
    }
    if parts.is_empty() {
        return path.to_string();
    }
    let query = parts
        .iter()
        .map(|(k, v)| format!("{}={}", k.to_lowercase(), v))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{}", path, query)
}

/// The upstream fetch is canonical too: one payload per cache key, fetched
/// with generous params, then shaped per request by `shape_canonical_hubs`.
/// This keeps PMS's expensive regenerations to one per directory selection.
fn canonical_fetch_path(req: &Request, key_path: &str) -> String {
    let queries = req.queries();
    let mut parts: Vec<(String, String)> = vec![];
    for (lower, orig) in [
        ("contentdirectoryid", "contentDirectoryID"),
        ("pinnedcontentdirectoryid", "pinnedContentDirectoryID"),
        ("x-plex-container-start", "X-Plex-Container-Start"),
        ("x-plex-container-size", "X-Plex-Container-Size"),
    ] {
        if let Some(v) = queries.get(orig).or_else(|| queries.get(lower)) {
            parts.push((orig.to_string(), v.clone()));
        }
    }
    let mut query: Vec<String> =
        parts.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if !query.iter().any(|p| p.starts_with("includeGuids")) {
        query.push("includeGuids=1".to_string());
    }
    if !query.iter().any(|p| p.starts_with("count=")) {
        query.push("count=50".to_string());
    }
    let base = key_path.split('?').next().unwrap_or(key_path);
    format!("{}?{}", base, query.join("&"))
}

/// Shape the canonical payload for this specific request: honor
/// excludeContinueWatching and per-row count limits clients asked for.
fn shape_canonical_hubs(
    container: &mut MediaContainerWrapper<MediaContainer>,
    context: &PlexContext,
) {
    let hubs = container.media_container.children_mut();
    if context.exclude_continue_watching {
        hubs.retain(|h| {
            !h.hub_identifier
                .as_deref()
                .map(|id| id.starts_with("home.continue"))
                .unwrap_or(false)
        });
    }
    if let Some(count) = context.count {
        for hub in hubs {
            let children = hub.children_mut();
            if children.len() > count as usize {
                children.truncate(count as usize);
                hub.size = Some(count);
            }
        }
    }
}

#[handler]
pub async fn cached_hubs_response(
    req: &mut Request,
    res: &mut Response,
    depot: &mut Depot,
) -> Result<(), anyhow::Error> {
    let compatibility_passthrough = skip_optional_transforms(depot);

    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());

    let key_path = cache_key_for_request(req);
    let fetch_path = canonical_fetch_path(req, &key_path);
    let config = app_config(depot)?;

    // Raw hub payloads are always account scoped. The canonical path removes
    // client-shaping noise while `hub_cache_key` adds a token hash, so the
    // warmer and live requests share entries only within the same account.
    let cache_key =
        crate::hub_cache::hub_cache_key(&key_path, context.token.as_deref());
    let mut container: MediaContainerWrapper<MediaContainer> =
        match plex_client.cache.get(&cache_key).await {
            Some(cached) => {
                if let Ok(state) = state::from_depot(depot) {
                    state.metrics.cache_hit();
                }
                // Stale-while-revalidate: serve immediately, refresh in the
                // background so nobody waits on a slow upstream fetch.
                if config.hub_stale_ttl > 0
                    && crate::hub_cache::is_stale(
                        &cache_key,
                        Duration::from_secs(config.hub_stale_ttl),
                    )
                    .await
                {
                    tracing::debug!(
                        path = %key_path,
                        "serving stale hubs, refreshing in background"
                    );
                    crate::hub_cache::spawn_hub_refresh(
                        plex_client.clone(),
                        fetch_path.clone(),
                        cache_key.clone(),
                    );
                }
                cached
            }
            None => {
                if let Ok(state) = state::from_depot(depot) {
                    state.metrics.cache_miss();
                }
                let parsed = crate::hub_cache::fetch_hubs_payload(
                    &plex_client,
                    &fetch_path,
                )
                .await?;
                plex_client
                    .cache
                    .insert(cache_key.clone(), parsed.clone())
                    .await;
                crate::hub_cache::track_fetched(cache_key.clone()).await;
                parsed
            }
        };

    container.content_type = content_type;

    let hubs_before = container.media_container.children().len();

    // Collection visibility is account policy, not presentation behaviour.
    // It must run even for Plexamp / Live TV compatibility requests so a
    // client-controlled product or path cannot reveal a hidden collection.
    TransformBuilder::new(plex_client.clone(), context.clone())
        .with_transform(CollectionVisibilityTransform)
        .apply_to(&mut container)
        .await;

    if !compatibility_passthrough {
        TransformBuilder::new(plex_client, context.clone())
            .with_transform(HubRestrictionTransform)
            .with_transform(HubStyleTransform { is_home: true })
            .with_transform(HubWatchedTransform)
            .with_transform(HubInterleaveTransform)
            .with_transform(UserStateTransform)
            .with_transform(HubKeyTransform)
            .apply_to(&mut container)
            .await;

        shape_canonical_hubs(&mut container, &context);
    }

    tracing::debug!(
        path = %req.uri().path(),
        path = %key_path,
        hubs_before,
        hubs_after = container.media_container.children().len(),
        "hub payload transformed"
    );

    res.render(container);
    Ok(())
}

#[handler]
pub async fn transform_hubs_response(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());

    let mut container: MediaContainerWrapper<MediaContainer> =
        from_salvo_response(res).await?;
    container.content_type = content_type;

    TransformBuilder::new(plex_client, context.clone())
        .with_transform(CollectionVisibilityTransform)
        .with_transform(HubRestrictionTransform)
        .with_transform(HubStyleTransform { is_home: true })
        .with_transform(HubWatchedTransform)
        .with_transform(HubInterleaveTransform)
        .with_transform(UserStateTransform)
        .with_transform(HubKeyTransform)
        .apply_to(&mut container)
        .await;

    res.render(container);
    Ok(())
}

#[handler]
pub async fn transform_req_content_directory(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    if skip_optional_transforms(depot) {
        return;
    }

    let config = match app_config(depot) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %error, "content directory transform missing configuration");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };

    // Interleave disabled: pass requests through untouched so clients get
    // native per-library hub responses.
    if !config.interleave {
        return;
    }

    let context = match request_context(depot) {
        Ok(context) => context.plex.clone(),
        Err(error) => {
            tracing::warn!(error = %error, "content directory transform missing request context");
            res.status_code(StatusCode::BAD_REQUEST);
            return;
        }
    };
    let _plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            res.status_code(StatusCode::UNAUTHORIZED);
            return;
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());

    // Old clients send both contentDirectoryID and pinnedContentDirectoryID.
    // The new experience sends neither — in that case we let the request pass
    // through unmodified so the proxy doesn't return an empty container.
    if let Some(ref pinned) = context.clone().pinned_content_directory_id {
        let Some(first_pinned) = pinned.first() else {
            tracing::debug!("Ignoring empty pinnedContentDirectoryID list");
            return;
        };
        // Mobile/tv clients fire only ONE promoted request per refresh (a
        // single slot). Giving them the intentional-empty second-slot
        // response leaves their home screen with nothing on it, so they
        // always get the full merged payload instead. Web-style clients
        // fetch every slot and rely on the empty non-first responses to
        // avoid duplicated rows, so keep stock behaviour for them.
        let mobile_client = matches!(
            context.platform.clone().unwrap_or_default(),
            Platform::Ios | Platform::Android | Platform::TvOS
        );

        let is_first = match context.clone().content_directory_id.as_deref() {
            // Aggregate request covering every pinned directory (modern web
            // clients send one combined call): always serve the full merge.
            Some(c) if c.len() > 1 => true,
            // Legacy clients page through one slot at a time; only the first
            // pinned slot carries the merged payload.
            Some(c) => c.first().is_some_and(|first| first == first_pinned),
            // Absent → treat as first directory.
            None => true,
        };

        tracing::debug!(
            path = %req.uri().path(),
            is_first,
            mobile_client,
            "promoted slot decision"
        );

        if !is_first && !mobile_client {
            // Not the first directory: return empty container so only the
            // first slot triggers a full merged fetch.
            tracing::debug!(
                path = %req.uri().path(),
                "promoted non-first slot, serving intentional empty"
            );
            let mut container: MediaContainerWrapper<MediaContainer> =
                MediaContainerWrapper {
                    content_type: content_type.clone(),
                    ..Default::default()
                };
            container.media_container.size = Some(0);
            container.media_container.identifier =
                Some("com.plexapp.plugins.library".to_string());
            res.render(container);
            ctrl.skip_rest();
            return;
        }

        // First directory (or mobile client): expand contentDirectoryID to
        // all pinned IDs so HubInterleaveTransform can merge hubs from every
        // library. Repeat mobile fetches are served from cache.
        add_query_param_salvo(
            req,
            "contentDirectoryID".to_string(),
            pinned.iter().join(","),
        );
    }
    // pinnedContentDirectoryID absent → new experience, pass through as-is.
}

#[handler]
pub async fn transform_req_include_guids(
    req: &mut Request,
    depot: &mut Depot,
    _res: &mut Response,
) {
    if skip_optional_transforms(depot) {
        return;
    }

    add_query_param_salvo(req, "includeGuids".to_string(), "1".to_string());
}

// some androids have trouble loading more for hero style. So load more at once
#[handler]
pub async fn transform_req_android(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) {
    if skip_optional_transforms(depot) {
        return;
    }

    let config = match app_config(depot) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %error, "Android request transform missing configuration");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    let context = match request_context(depot) {
        Ok(context) => context.plex.clone(),
        Err(error) => {
            tracing::warn!(error = %error, "Android request transform missing request context");
            res.status_code(StatusCode::BAD_REQUEST);
            return;
        }
    };

    let mut count = context.clone().count.unwrap_or(25);
    if context.platform.unwrap_or_default() == Platform::Android {
        count = 50
    }
    // Hack, as the list could be smaller when removing watched items. So we request more.
    if config.exclude_watched && count < 50 {
        count = 50;
    }

    add_query_param_salvo(req, "count".to_string(), count.to_string());
}

// rhis handles refresh of individual rows or paging and paging if it
#[handler]
pub async fn get_collections_children(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let config = app_config(depot)?;
    let context = request_context(depot)?.plex.clone();
    let collection_ids = req
        .param::<String>("ids")
        .ok_or_else(|| anyhow::anyhow!("missing collection ids"))?;
    let collection_ids: Vec<u32> = collection_ids
        .split(',')
        .filter_map(|v| v.parse::<u32>().ok())
        .collect();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());

    let mut limit: i32 = 250;
    let mut offset: i32 = 0;
    if !config.exclude_watched {
        limit = context.container_size.unwrap_or(50);
        offset = context.container_start.unwrap_or(0);
    }

    // create a stub
    let mut container: MediaContainerWrapper<MediaContainer> =
        MediaContainerWrapper {
            content_type,
            ..Default::default()
        };
    let size = container.media_container.children().len();
    container.media_container.size =
        Some(i64::try_from(size).unwrap_or(i64::MAX));
    container.media_container.offset = Some(offset);

    // filtering of watched happens in the transform
    TransformBuilder::new(plex_client, context.clone())
        .with_transform(LibraryInterleaveTransform {
            collection_ids: collection_ids.clone(),
            offset,
            limit,
        })
        .with_transform(HubRestrictionTransform)
        .with_transform(CollectionStyleTransform {
            collection_ids: collection_ids.clone(),
            hub: context.content_directory_id.is_some() // its a guessing game
                && !context.include_collections
                && !context.include_advanced
                && !context.exclude_all_leaves,
        })
        .with_transform(UserStateTransform)
        //.with_transform(MediaContainerScriptingTransform)
        .apply_to(&mut container)
        .await;

    res.render(container); // TODO: FIx XML
    Ok(())
}

#[handler]
pub async fn default_transform(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());
    let style = req
        .param::<Style>("style")
        .ok_or_else(|| anyhow::anyhow!("missing or invalid style"))?;
    let rest_path = req
        .param::<String>("**rest")
        .ok_or_else(|| anyhow::anyhow!("missing transform path"))?;

    let mut url = Url::parse(req.uri().to_string().as_str())?;
    url.set_path(&rest_path);
    req.set_uri(hyper::Uri::try_from(url.as_str())?);

    // patch, plex seems to pass wrong contentdirid, probaply cause we all load it inti the first
    let mut queries = req.queries().clone();
    queries.remove("contentDirectoryID");
    replace_query(queries, req);

    let upstream_res = plex_client.request(req).await?;
    match upstream_res.status() {
        reqwest::StatusCode::OK => (),
        status => {
            tracing::error!(status = ?status, path = %req.uri().path(), "Failed to get plex response");
            return Err(
                salvo::http::StatusError::internal_server_error().into()
            );
        }
    };

    let mut container: MediaContainerWrapper<MediaContainer> =
        from_reqwest_response(upstream_res).await?;
    container.content_type = content_type;

    TransformBuilder::new(plex_client, context.clone())
        .with_transform(HubRestrictionTransform)
        .with_transform(MediaStyleTransform { style })
        .with_transform(UserStateTransform)
        .with_transform(HubWatchedTransform)
        .with_transform(HubKeyTransform)
        .apply_to(&mut container)
        .await;

    res.render(container);
    Ok(())
}

#[handler]
pub async fn get_library_item_metadata(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) {
    let config = match app_config(depot) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %error, "library metadata handler missing configuration");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    let context = match request_context(depot) {
        Ok(context) => context.plex.clone(),
        Err(error) => {
            tracing::warn!(error = %error, "library metadata handler missing request context");
            res.status_code(StatusCode::BAD_REQUEST);
            return;
        }
    };
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            res.status_code(StatusCode::UNAUTHORIZED);
            return;
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());

    if config.disable_related {
        add_query_param_salvo(
            req,
            "includeRelated".to_string(),
            "0".to_string(),
        );
    }

    let upstream_res = match plex_client.request(req).await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, path = %req.uri().path(), "Plex upstream request failed");
            res.status_code(StatusCode::BAD_GATEWAY);
            return;
        }
    };
    let mut container: MediaContainerWrapper<MediaContainer> =
        match from_reqwest_response(upstream_res).await {
            Ok(r) => r,
            Err(error) => {
                tracing::error!(error = ?error, path = %req.uri().path(), "Failed to get plex response");
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                return;
            }
        };
    container.content_type = content_type;

    TransformBuilder::new(plex_client, context.clone())
        //.with_transform(MediaContainerScriptingTransform)
        .apply_to(&mut container)
        .await;
    // dbg!(container.media_container.count);
    res.render(container);
}

// const RESOLUTIONS: HashMap<&'static str, &'static str> =
//     HashMap::from([("1080p", "1920x1080"), ("4k", "4096x2160")]);

#[handler]
async fn force_maximum_quality(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<(), anyhow::Error> {
    if skip_optional_transforms(depot) {
        return Ok(());
    }

    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let config = app_config(depot)?;
    let mut queries = req.queries().clone();

    if queries.get("maxVideoBitrate").is_none()
        && queries.get("videoBitrate").is_none()
    {
        return Ok(());
    }

    queries.remove("maxVideoBitrate");
    queries.remove("videoBitrate");
    queries.remove("autoAdjustQuality");
    queries.insert("autoAdjustQuality".to_string(), "0".to_string());
    queries.remove("directStream");
    queries.insert("directStream".to_string(), "1".to_string());
    queries.remove("directPlay");
    queries.insert("directPlay".to_string(), "1".to_string());
    queries.remove("videoQuality");
    queries.insert("videoQuality".to_string(), "100".to_string());
    // queries.insert("directStreamAudio".to_string(), "0".to_string());
    //queries.remove("videoResolution");
    //queries.insert("videoResolution".to_string(), "4096x2160".to_string());

    // some clients send wrong buffer format
    if let Some(size) = queries.remove("mediaBufferSize") {
        if let Some(raw_size) = size.first() {
            match raw_size.parse::<f32>() {
                Ok(parsed) => {
                    queries.insert(
                        "mediaBufferSize".to_string(),
                        (parsed as i64).to_string(),
                    );
                }
                Err(error) => {
                    tracing::warn!(value = %raw_size, error = %error, "Ignoring malformed mediaBufferSize");
                    queries.insert(
                        "mediaBufferSize".to_string(),
                        raw_size.clone(),
                    );
                }
            }
        }
    }
    // if let Some(i) = req.queries().get("protocol") {
    //     if i == "http" {
    //         queries.remove("copyts");
    //         queries.insert("copyts".to_string(), "0".to_string());
    //         queries.remove("hasMDE");
    //         queries.insert("hasMDE".to_string(), "0".to_string());
    //     }
    // }

    let query_key = "X-Plex-Client-Profile-Extra".to_string();
    if let Some(extra) = queries
        .remove(&query_key)
        .and_then(|values| values.into_iter().next())
    {
        let filtered_extra = extra
            .split('+')
            .filter(|s| {
                !s.contains("add-limitation")
                    && !s.to_lowercase().contains("name=video.bitrate")
            })
            .join("+");

        queries.insert(query_key, filtered_extra);
    };

    if let (Some(resos), Some(path)) =
        (config.force_direct_play_for.as_ref(), queries.get("path"))
    {
        let item = match plex_client
            .clone()
            .get_item_by_key(path.to_string())
            .await
        {
            Ok(item) => item,
            Err(error) => {
                tracing::warn!(error = %error, "Unable to load direct-play metadata");
                return Ok(());
            }
        };

        let media_index = req
            .queries()
            .get("mediaIndex")
            .filter(|value| *value != "-1")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);

        let Some(media_item) = item
            .media_container
            .metadata
            .first()
            .and_then(|metadata| metadata.media.get(media_index))
            .cloned()
        else {
            tracing::warn!(
                media_index,
                "Ignoring invalid mediaIndex in force maximum quality request"
            );
            return Ok(());
        };

        for reso in resos {
            if let Some(video_resolution) = media_item.video_resolution.clone()
            {
                if video_resolution.to_lowercase() == reso.to_lowercase() {
                    queries.remove("directPlay");
                    queries.insert("directPlay".to_string(), "1".to_string());
                    queries.remove("videoResolution");
                    // queries.insert(
                    //     "videoResolution".to_string(),
                    //     RESOLUTIONS.get(&reso.to_lowercase()),
                    // );
                }
            }
        }
    }

    replace_query(queries, req);
    Ok(())
}

// async fn execute_video_transcode_fallback(
//     req: &mut Request,
//     item: MediaContainerWrapper<MediaContainer>,
//     media_index: usize,
// ) -> Result<(), anyhow::Error> {
//     let context: PlexContext = req.extract().await.unwrap();
//     let plex_client = plex_client_from_depot(&context, depot);
//     let mut queries = req.queries().clone();
//     let mut original_queries = req.queries().clone();

//     let response = plex_client.request(req).await?;
//     let mut transcode: MediaContainerWrapper<MediaContainer> =
//         from_reqwest_response(response).await?;

//     let streams =
//         &transcode.media_container.metadata[0].media[media_index].parts[0].streams;
//     let selected_media = transcode.media_container.metadata[0].media[media_index].clone();
//     let mut fallback_selected = false;
//     for stream in streams {
//         if stream.stream_type.clone().unwrap() == 1
//             && stream.decision.clone().unwrap_or("unknown".to_string())
//                 == "transcode"
//         {
//             tracing::trace!(
//                 "{} is transcoding, looking for fallback",
//                 selected_media
//             );
//             // for now just select a random fallback
//             for (index, media) in
//                 item.media_container.metadata[0].media.iter().enumerate()
//             {
//                 if transcode.media_container.metadata[0].media[media_index].id != media.id
//                 {
//                     tracing::debug!(
//                         "Video transcode fallback from {} to {}",
//                         selected_media,
//                         media,
//                     );
//                     queries.remove("mediaIndex");
//                     queries.insert("mediaIndex".to_string(), index.to_string());
//                     queries.remove("directPlay");
//                     queries.insert("directPlay".to_string(), "0".to_string());
//                     queries.remove("directStream");
//                     queries.insert("directStream".to_string(), "1".to_string());
//                     fallback_selected = true;
//                     break;
//                 }
//             }
//         }
//     }
//     if !fallback_selected {
//         replace_query(original_queries, req);
//     }
//     Ok(())
// }

pub struct TranscodingStatus {
    pub is_transcoding: bool,
    pub decision_result: MediaContainerWrapper<MediaContainer>,
}

async fn get_transcoding_for_request(
    req: &mut Request,
    depot: &Depot,
) -> Result<TranscodingStatus, anyhow::Error> {
    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };

    let response = plex_client.proxy_request(req).await?;
    let transcode: MediaContainerWrapper<MediaContainer> =
        from_reqwest_response(response).await?;
    let mut is_transcoding = false;

    if transcode.media_container.size == Some(0) {
        return Ok(TranscodingStatus {
            is_transcoding,
            decision_result: transcode,
        });
    }

    is_transcoding =
        crate::playback_selection::decision_is_transcoding(&transcode);

    Ok(TranscodingStatus {
        is_transcoding,
        decision_result: transcode,
    })
}

// TODO: Fallback to a version close to the requested bitrate
#[handler]
async fn video_transcode_fallback(
    req: &mut salvo::Request,
    depot: &mut Depot,
    _res: &mut salvo::Response,
    _ctrl: &mut FlowCtrl,
) -> Result<(), anyhow::Error> {
    if skip_optional_transforms(depot) {
        return Ok(());
    }

    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let config = app_config(depot)?;
    let mut queries = req.queries().clone();
    let original_queries = req.queries().clone();
    let Some(media_index) = crate::playback_selection::requested_media_index(
        req.queries().get("mediaIndex").map(String::as_str),
    ) else {
        tracing::warn!("Ignoring malformed mediaIndex in fallback request");
        return Ok(());
    };

    let Some(fallback_for) = config
        .video_transcode_fallback_for
        .as_ref()
        .and_then(|values| values.first())
        .map(|value| value.to_lowercase())
    else {
        return Ok(());
    };

    let Some(path) = req.queries().get("path").cloned() else {
        return Ok(());
    };
    let item = match plex_client.clone().get_item_by_key(path).await {
        Ok(item) => item,
        Err(error) => {
            tracing::warn!(error = %error, "Unable to load fallback metadata");
            return Ok(());
        }
    };
    let Some(media) = item
        .media_container
        .metadata
        .first()
        .map(|metadata| metadata.media.as_slice())
    else {
        tracing::warn!("Fallback metadata contained no item");
        return Ok(());
    };
    let Some(selected_media) = media.get(media_index) else {
        tracing::warn!(
            media_index,
            "Ignoring out-of-range fallback mediaIndex"
        );
        return Ok(());
    };

    if !selected_media
        .video_resolution
        .as_deref()
        .is_some_and(|resolution| {
            resolution.eq_ignore_ascii_case(&fallback_for)
        })
    {
        tracing::debug!("Media item not marked for fallback, continue playing");
        return Ok(());
    }

    if media.len() <= 1 {
        tracing::debug!("Nothing to fallback on, skipping fallback check");
    } else {
        let requested_bitrate = queries
            .get("videoBitrate")
            .or_else(|| queries.get("maxVideoBitrate"))
            .and_then(|value| value.parse::<i64>().ok());
        let status = get_transcoding_for_request(req, depot).await?;
        if !status.is_transcoding {
            return Ok(());
        }

        let policy = match security_state(depot) {
            Some(SecurityContextState::Resolved(security)) => {
                Some(security.policy.clone())
            }
            Some(SecurityContextState::Unavailable {
                fail_closed: true,
                ..
            }) => {
                tracing::warn!(
                    "Identity unavailable; suppressing fallback candidates"
                );
                None
            }
            _ => None,
        };
        let candidates = if config.resolution_policy_enabled
            && matches!(
                security_state(depot),
                Some(SecurityContextState::Unavailable {
                    fail_closed: true,
                    ..
                })
            ) {
            Vec::new()
        } else {
            crate::playback_selection::fallback_indexes(
                media,
                media_index,
                &fallback_for,
                policy.as_ref(),
            )
        };
        let Some(fallback_index) = candidates.first().copied() else {
            tracing::debug!("No suitable fallback found");
            replace_query(original_queries, req);
            return Ok(());
        };
        let fallback = &media[fallback_index];
        tracing::debug!(
            from_media_id = selected_media.id,
            to_media_id = fallback.id,
            fallback_index,
            "Video transcode fallback selected"
        );
        queries.remove("mediaIndex");
        queries.insert("mediaIndex".to_string(), fallback_index.to_string());
        queries.remove("directStream");
        queries.insert("directStream".to_string(), "1".to_string());
        if requested_bitrate.is_none() {
            queries.remove("directPlay");
            queries.insert("directPlay".to_string(), "1".to_string());
        }
        queries.remove("subtitles");
        queries.insert("subtitles".to_string(), "auto".to_string());
        replace_query(queries, req);
    }

    // replace_query(queries, req);
    Ok(())
}

/// Enforce the authenticated account's resolution limit on playback requests.
///
/// Runs before every other version-selection hoop. For restricted accounts:
/// - a prohibited explicit `mediaIndex` is rewritten to the best permitted
///   version, so manual selection cannot bypass the policy
/// - an absent `mediaIndex` is set explicitly to the best permitted version,
///   which also keeps downstream auto selection inside the policy
/// - if nothing is permitted the request is rejected with 403
///
/// Unrestricted accounts return immediately with no changes.
#[handler]
async fn enforce_resolution_policy(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) -> Result<(), anyhow::Error> {
    let runtime_policy = state::from_depot(depot)?.policy.snapshot().await;
    if !runtime_policy.enabled {
        return Ok(());
    }

    let context = request_context(depot)?.plex.clone();
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };

    let (identity, policy) = match security_state(depot) {
        Some(SecurityContextState::Resolved(security)) => {
            (security.identity.clone(), security.policy.clone())
        }
        Some(SecurityContextState::Unavailable {
            fail_closed,
            reason,
        }) => {
            if fail_closed {
                tracing::warn!(
                    error = %reason,
                    "Identity unavailable, failing closed on playback request"
                );
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                ctrl.skip_rest();
                return Ok(());
            }
            tracing::warn!(
                error = %reason,
                "Identity unavailable, failing open on playback request"
            );
            return Ok(());
        }
        None => {
            let identity = plex_client.get_current_user().await?;
            let policy = resolve_policy(
                &runtime_policy.entries,
                runtime_policy.default_limit,
                &runtime_policy.hidden_collections,
                &identity,
            );
            (identity, policy)
        }
    };
    if policy.is_unrestricted() && policy.max_bitrate.is_none() {
        return Ok(());
    }

    // Per-user bitrate cap. Independent of the resolution limit, so a user
    // can be resolution-unrestricted yet bandwidth-capped.
    if let Some(cap) = policy.max_bitrate {
        let mut queries = req.queries().clone();
        let current = queries
            .get("maxVideoBitrate")
            .or_else(|| queries.get("videoBitrate"))
            .and_then(|v| v.parse::<i64>().ok());
        let effective = current.map_or(cap, |c| c.min(cap));
        if current.map(|c| c > effective).unwrap_or(true) {
            tracing::info!(
                username = %identity.username,
                requested = ?current,
                capped_to = effective,
                "Capping video bitrate"
            );
            queries.remove("videoBitrate");
            queries.remove("maxVideoBitrate");
            queries
                .insert("maxVideoBitrate".to_string(), effective.to_string());
            replace_query(queries, req);
        }
    }

    if policy.is_unrestricted() {
        return Ok(());
    }

    let mut queries = req.queries().clone();
    let path = match queries.get("path") {
        Some(path) => path.to_string(),
        None => return Ok(()),
    };

    let item = match plex_client.get_item_by_key(path).await {
        Ok(item) => item,
        Err(error) => {
            tracing::warn!(error = ?error, "Could not load item for resolution policy");
            if runtime_policy.fail_closed {
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                ctrl.skip_rest();
                return Ok(());
            }
            return Ok(());
        }
    };

    let Some(metadata) = item.media_container.metadata.first() else {
        tracing::warn!("Plex item response contained no metadata");
        if runtime_policy.fail_closed {
            res.status_code(StatusCode::BAD_GATEWAY);
            ctrl.skip_rest();
        }
        return Ok(());
    };
    let media = &metadata.media;
    if media.is_empty() {
        return Ok(());
    }

    // Record immutable media classification facts for each part. Direct part
    // requests evaluate the current account policy against these facts, so a
    // later policy change takes effect immediately without stale permission
    // booleans in the cache.
    plex_client.cache_part_classification(media).await;

    let screen_resolution = context
        .screen_resolution
        .first()
        .map(|r| (r.width, r.height));

    let requested_index: Option<usize> = queries
        .get("mediaIndex")
        .filter(|v| *v != "-1")
        .and_then(|v| v.parse::<usize>().ok());

    match requested_index {
        Some(index) => {
            let allowed = media
                .get(index)
                .map(|m| media_allowed(m, &policy))
                .unwrap_or(false);
            if allowed {
                tracing::debug!(
                    username = %identity.username,
                    media_index = index,
                    "Requested version within policy"
                );
                return Ok(());
            }
            tracing::info!(
                username = %identity.username,
                requested = index,
                maximum = ?policy.limit,
                "Blocked media version"
            );
        }
        None => {
            tracing::debug!(
                username = %identity.username,
                maximum = ?policy.limit,
                "Selecting version within policy"
            );
        }
    }

    // Either the requested version was prohibited or none was requested:
    // pick the best permitted one and pin it explicitly.
    match best_allowed_media(media, &policy, screen_resolution) {
        Some(best) => {
            let Some(best_index) = media.iter().position(|m| m.id == best.id)
            else {
                tracing::warn!("Selected media disappeared before rewrite");
                return Ok(());
            };
            tracing::info!(
                username = %identity.username,
                replacement = best_index,
                maximum = ?policy.limit,
                "Rewriting mediaIndex to permitted version"
            );
            queries.remove("mediaIndex");
            queries.insert("mediaIndex".to_string(), best_index.to_string());
            replace_query(queries, req);
            Ok(())
        }
        None => {
            if let Ok(state) = state::from_depot(depot) {
                state.metrics.policy_reject();
            }
            tracing::info!(
                username = %identity.username,
                maximum = ?policy.limit,
                "No permitted version exists, rejecting playback"
            );
            res.status_code(StatusCode::FORBIDDEN);
            ctrl.skip_rest();
            Ok(())
        }
    }
}

/// When multiple qualities are avaiable, select the most relevant one.
/// Does not work for every client as some client decides themselfs which version to use.
#[handler]
async fn auto_select_version(req: &mut Request, depot: &mut Depot) {
    if skip_optional_transforms(depot) {
        return;
    }

    let context = match request_context(depot) {
        Ok(context) => context.plex.clone(),
        Err(error) => {
            tracing::warn!(error = %error, "Auto selection missing request context");
            return;
        }
    };
    let plex_client = match plex_client_from_depot(&context, depot) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return;
        }
    };
    let mut queries = req.queries().clone();
    let media_index = queries.get("mediaIndex");

    if media_index.is_some_and(|index| index != "-1") {
        tracing::debug!(
            "Skipping auto selected as client specified a media index"
        );
        return;
    }

    let Some(screen) = context.screen_resolution.first() else {
        tracing::debug!(
            "Skipping auto selected as no screen resolution has been specified"
        );
        return;
    };

    if queries.get("path").is_some() {
        let Some(path) = req.queries().get("path").cloned() else {
            return;
        };
        let item = match plex_client.get_item_by_key(path).await {
            Ok(item) => item,
            Err(error) => {
                tracing::warn!(error = %error, "Unable to load auto-selection metadata");
                return;
            }
        };
        let Some(media) = item
            .media_container
            .metadata
            .first()
            .map(|metadata| metadata.media.as_slice())
        else {
            return;
        };
        if media.len() <= 1 {
            tracing::debug!(
                "Only one media version available, skipping auto select"
            );
            return;
        }

        let requested_bitrate = queries
            .get("videoBitrate")
            .or_else(|| queries.get("maxVideoBitrate"))
            .and_then(|value| value.parse::<i64>().ok());
        if let Some(index) = crate::playback_selection::best_fit_index(
            media,
            (screen.width, screen.height),
        ) {
            tracing::debug!(media_index = index, "Auto selected media version");
            queries.remove("mediaIndex");
            queries.insert("mediaIndex".to_string(), index.to_string());
            if requested_bitrate.is_none() {
                queries.remove("directPlay");
                queries.insert("directPlay".to_string(), "1".to_string());
            }
            queries.remove("subtitles");
            queries.insert("subtitles".to_string(), "auto".to_string());
        }
    }
    replace_query(queries, req);
}

fn admin_authorized(depot: &Depot) -> bool {
    let Ok(state) = state::from_depot(depot) else {
        return false;
    };
    let Ok(request) = request_context(depot) else {
        return false;
    };
    match (state.config.token.as_deref(), request.plex.token.as_deref()) {
        (Some(configured), Some(presented)) => {
            crate::account_scope::token_fingerprint(configured)
                == crate::account_scope::token_fingerprint(presented)
        }
        _ => false,
    }
}

fn require_admin(depot: &Depot, res: &mut Response) -> bool {
    if admin_authorized(depot) {
        true
    } else {
        res.status_code(StatusCode::FORBIDDEN);
        false
    }
}

#[handler]
async fn liveness(res: &mut Response) {
    res.render(Json(serde_json::json!({"status": "alive"})));
}

#[handler]
async fn readiness(depot: &mut Depot, res: &mut Response) {
    if !require_admin(depot, res) {
        return;
    }
    let Ok(state) = state::from_depot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    let Some(host) = state.config.host.as_deref() else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    let identity_url = format!("{}/:/identity", host.trim_end_matches('/'));
    let mut request = state.identity_http.get(identity_url);
    if let Some(token) = state.config.token.as_deref() {
        request = request.header("X-Plex-Token", token);
    }
    let plex_status = request.send().await.map(|response| response.status());
    let ready = plex_status.as_ref().is_ok_and(|status| status.is_success());
    if !ready {
        state.metrics.upstream_failure();
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
    }
    res.render(Json(serde_json::json!({
        "status": if ready { "ready" } else { "not_ready" },
        "plex": plex_status.map(|status| status.as_u16()).ok(),
        "cache": {
            "metadata_entries": state.metadata_cache.entry_count(),
            "identity_entries": state.identity_cache.entry_count(),
            "part_entries": state.part_media_cache.entry_count(),
            "disk_bytes": crate::disk_cache::current_size()
        },
        "jobs": state.jobs.snapshot().await,
        "policy": state.policy.snapshot().await
    })));
}

#[handler]
async fn metrics(depot: &mut Depot, res: &mut Response) {
    if !require_admin(depot, res) {
        return;
    }
    let Ok(state) = state::from_depot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    res.render(Text::Plain(state.metrics.prometheus()));
}

#[handler]
async fn cache_status(depot: &mut Depot, res: &mut Response) {
    if !require_admin(depot, res) {
        return;
    }
    let Ok(state) = state::from_depot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    res.render(Json(serde_json::json!({
        "metadata": state.metadata_cache.entry_count(),
        "identity": state.identity_cache.entry_count(),
        "parts": state.part_media_cache.entry_count(),
        "photos": PHOTO_CACHE.entry_count(),
        "global": crate::cache::GLOBAL_CACHE.inner.entry_count(),
        "disk_bytes": crate::disk_cache::current_size()
    })));
}

#[handler]
async fn cache_purge(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    if !require_admin(depot, res) {
        return;
    }
    let Ok(state) = state::from_depot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    let class = req.param::<String>("class").unwrap_or_default();
    let all_accounts = req.query::<String>("account").as_deref() == Some("all");
    let scope = request_context(depot)
        .ok()
        .map(|context| context.token_scope.clone())
        .unwrap_or_else(|| "anon".to_string());
    let mut removed = 0u64;
    match class.as_str() {
        "metadata" => {
            if all_accounts {
                removed = state.metadata_cache.entry_count();
                state.metadata_cache.invalidate_all();
            } else {
                for (key, _) in state.metadata_cache.iter() {
                    if key.contains(&format!(":u:{scope}:")) {
                        state.metadata_cache.invalidate(key.as_ref()).await;
                        removed += 1;
                    }
                }
            }
        }
        "identity" => {
            let fingerprint = request_context(depot).ok().and_then(|context| {
                context
                    .plex
                    .token
                    .as_deref()
                    .map(crate::account_scope::token_fingerprint)
            });
            if all_accounts {
                removed = state.identity_cache.entry_count();
                state.identity_cache.invalidate_all();
            } else if let Some(fingerprint) = fingerprint {
                state.identity_cache.invalidate(&fingerprint).await;
                removed = 1;
            }
        }
        "photos" => {
            if all_accounts {
                removed = PHOTO_CACHE.entry_count();
                PHOTO_CACHE.invalidate_all();
            } else {
                for (key, _) in PHOTO_CACHE.iter() {
                    if key.contains(&format!(":u:{scope}:")) {
                        PHOTO_CACHE.invalidate(key.as_ref()).await;
                        removed += 1;
                    }
                }
            }
        }
        "parts" if all_accounts => {
            removed = state.part_media_cache.entry_count();
            state.part_media_cache.invalidate_all();
        }
        "global" if all_accounts => {
            removed = crate::cache::GLOBAL_CACHE.inner.entry_count();
            let _ = crate::cache::GLOBAL_CACHE.clear().await;
        }
        "disk" if all_accounts => match crate::disk_cache::clear_all().await {
            Ok(count) => removed = count,
            Err(error) => {
                tracing::warn!(error = %error, "Disk cache purge failed");
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                return;
            }
        },
        "parts" | "global" | "disk" => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(serde_json::json!({
                "error": "this cache class is not account-scoped; use ?account=all"
            })));
            return;
        }
        _ => {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }
    }
    res.render(Json(
        serde_json::json!({"class": class, "removed": removed}),
    ));
}

#[handler]
async fn policy_reload(depot: &mut Depot, res: &mut Response) {
    if !require_admin(depot, res) {
        return;
    }
    let Ok(state) = state::from_depot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    let next: Config = match Config::figment().extract() {
        Ok(config) => config,
        Err(error) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(serde_json::json!({"error": error.to_string()})));
            return;
        }
    };
    if let Err(error) = next.validate() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(serde_json::json!({"error": error.to_string()})));
        return;
    }
    if next.resolution_policy_enabled != state.config.resolution_policy_enabled
    {
        res.status_code(StatusCode::CONFLICT);
        res.render(Json(serde_json::json!({
            "error": "enabling or disabling policy routing requires a restart"
        })));
        return;
    }
    let snapshot = state.policy.reload(&next).await;
    res.render(Json(snapshot));
}

#[handler]
async fn explain_playback(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) {
    if !require_admin(depot, res) {
        return;
    }
    let Some(path) = req.query::<String>("path") else {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    };
    let Ok(context) = request_context(depot) else {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    };
    let Ok(client) = plex_client_from_depot(&context.plex, depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    let item = match client.get_item_by_key(path.clone()).await {
        Ok(item) => item,
        Err(error) => {
            res.status_code(StatusCode::BAD_GATEWAY);
            res.render(Json(serde_json::json!({"error": error.to_string()})));
            return;
        }
    };
    let media = item
        .media_container
        .metadata
        .first()
        .map(|m| m.media.as_slice())
        .unwrap_or(&[]);
    let Ok(state) = state::from_depot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    let runtime = state.policy.snapshot().await;
    let policy = security_state(depot).and_then(|state| match state {
        SecurityContextState::Resolved(security) => {
            Some(security.policy.clone())
        }
        SecurityContextState::Unavailable { .. } => None,
    });
    let screen = context
        .plex
        .screen_resolution
        .first()
        .map(|s| (s.width, s.height));
    let selected = policy
        .as_ref()
        .and_then(|policy| best_allowed_media(media, policy, screen))
        .and_then(|best| {
            media.iter().position(|candidate| candidate.id == best.id)
        });
    let versions: Vec<_> = media.iter().enumerate().map(|(index, item)| serde_json::json!({
        "index": index,
        "id": item.id,
        "resolution": item.video_resolution,
        "width": item.width,
        "height": item.height,
        "allowed": policy.as_ref().map(|policy| media_allowed(item, policy)),
        "rejection_reason": policy.as_ref().and_then(|policy| (!media_allowed(item, policy)).then_some("exceeds policy or has unknown resolution"))
    })).collect();
    res.render(Json(serde_json::json!({
        "path": path,
        "policy_generation": runtime.generation,
        "policy_source": security_state(depot).map(|_| "request_identity").unwrap_or("unavailable"),
        "selected_media_index": selected,
        "versions": versions
    })));
}

#[handler]
async fn ping(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    res.render("pong!")
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use rstest::rstest;

    use salvo::test::{ResponseExt, TestClient};

    #[test]
    fn stream_delivery_matrix_is_security_first() {
        assert_eq!(
            stream_delivery(true, true),
            StreamDelivery::Proxy,
            "restricted accounts must proxy even when redirects are enabled"
        );
        assert_eq!(
            stream_delivery(true, false),
            StreamDelivery::Proxy,
            "restricted accounts must proxy when redirects are disabled"
        );
        assert_eq!(
            stream_delivery(false, true),
            StreamDelivery::Redirect,
            "unrestricted accounts redirect only when configured"
        );
        assert_eq!(
            stream_delivery(false, false),
            StreamDelivery::Proxy,
            "unrestricted accounts proxy when redirects are disabled"
        );
    }

    #[rstest]
    #[case::hubs_sections(
        "/hubs/sections/6",
        "tests/mock/out/hubs_sections_6.json"
    )]
    #[case::hubs_promoted(
        format!("{}?contentDirectoryID=6&pinnedContentDirectoryID=6,7", PLEX_HUBS_PROMOTED), "tests/mock/out/hubs_promoted_6.json")
    ]
    #[case::hubs_home_fallback(
        "/hubs/home?count=12",
        "tests/mock/out/hubs_home_fallback.json"
    )]
    #[tokio::test]
    async fn test_routes(#[case] path: String, #[case] expected_path: String) {
        let _ = tracing_subscriber::fmt::try_init();
        let mock_server = get_mock_server();
        // Hold the env lock and reset config for the whole test so sibling
        // tests mutating REPLEX_* vars can't race this request.
        let _env = crate::test_helpers::pin_default_env(
            mock_server.address().to_string().as_str(),
        );

        let service = Service::new(super::route());

        // Send a Host header like a real Plex request. `path` already starts
        // with a slash, so no separator is added here.
        let content =
            TestClient::get(format!("http://127.0.0.1:5800{}", &path))
                .add_header("HOST", mock_server.address().to_string(), true)
                .add_header("X-Plex-Token", "fakeID", true)
                .add_header("X-Plex-Client-Identifier", "fakeID", true)
                .add_header("Accept", "application/json", true)
                .send(&service)
                .await
                .take_string()
                .await
                .unwrap();

        // Compare as parsed JSON so formatting differences don't matter.
        // Run with BLESS=1 to overwrite the golden file with the current
        // response after reviewing the diff.
        let actual: serde_json::Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| {
                panic!("response is not valid JSON ({e}): {content}")
            });
        if std::env::var("BLESS").as_deref() == Ok("1") {
            std::fs::write(
                &expected_path,
                serde_json::to_string_pretty(&actual).unwrap(),
            )
            .unwrap();
            return;
        }
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(expected_path).unwrap(),
        )
        .unwrap();
        // Hero transforms emit absolute image URLs that embed the upstream
        // host:port; normalise ephemeral mock ports so comparisons are
        // deterministic.
        fn normalize(v: &serde_json::Value) -> serde_json::Value {
            let re = regex::Regex::new(r"127\.0\.0\.1:\d+").unwrap();
            match v {
                serde_json::Value::String(s) => serde_json::Value::String(
                    re.replace_all(s, "127.0.0.1:PORT").to_string(),
                ),
                serde_json::Value::Array(a) => {
                    serde_json::Value::Array(a.iter().map(normalize).collect())
                }
                serde_json::Value::Object(o) => serde_json::Value::Object(
                    o.iter()
                        .map(|(k, val)| (k.clone(), normalize(val)))
                        .collect(),
                ),
                other => other.clone(),
            }
        }
        assert_eq!(normalize(&actual), normalize(&expected));
    }

    #[tokio::test]
    async fn plexamp_hubs_still_enforce_collection_visibility() {
        let _ = tracing_subscriber::fmt::try_init();
        let server = httpmock::MockServer::start();

        let _identity = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v2/user")
                .header("X-Plex-Token", "limited-token");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"id": 2, "uuid": "uuid-limited", "username": "limited"}"#,
                );
        });
        let hubs = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/hubs/sections/99")
                .header("X-Plex-Token", "limited-token");
            then.status(200)
                .header("content-type", "application/json")
                .body_from_file("tests/mock/in/hubs_sections_6.json");
        });

        let _env = pin_default_env(&server.address().to_string());
        std::env::set_var("REPLEX_IDENTITY_API_BASE", server.base_url());
        std::env::set_var("REPLEX_RESOLUTION_POLICY_ENABLED", "true");
        std::env::set_var("REPLEX_RESOLUTION_DEFAULT", "unlimited");
        std::env::set_var("REPLEX_HIDDEN_COLLECTIONS", "Trending");

        let service = Service::new(super::route());
        let mut response =
            TestClient::get("http://127.0.0.1:5800/hubs/sections/99")
                .add_header("HOST", server.address().to_string(), true)
                .add_header("X-Plex-Token", "limited-token", true)
                .add_header("X-Plex-Client-Identifier", "limited-client", true)
                .add_header("X-Plex-Product", "Plexamp", true)
                .add_header("Accept", "application/json", true)
                .send(&service)
                .await;

        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(hubs.hits(), 1, "Plexamp hub request must fetch upstream");

        let body = response.take_string().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let titles: Vec<&str> = json["MediaContainer"]["Hub"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hub| hub["title"].as_str())
            .collect();

        assert!(
            !titles.contains(&"Trending"),
            "Plexamp compatibility mode must not reveal hidden collections: {json}"
        );
    }

    #[tokio::test]
    async fn section_hub_cache_isolated_between_account_tokens() {
        let server = httpmock::MockServer::start();
        let account_a = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/hubs/sections/77")
                .header("X-Plex-Token", "hub-account-a");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"MediaContainer":{"size":1,"Hub":[{"key":"/a","title":"Account A","type":"movie"}]}}"#);
        });
        let account_b = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/hubs/sections/77")
                .header("X-Plex-Token", "hub-account-b");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"MediaContainer":{"size":1,"Hub":[{"key":"/b","title":"Account B","type":"movie"}]}}"#);
        });

        let _env = pin_default_env(&server.address().to_string());
        let service = Service::new(super::route());

        async fn hub_title(service: &Service, token: &str) -> String {
            let mut response =
                TestClient::get("http://127.0.0.1:5800/hubs/sections/77")
                    .add_header("HOST", "127.0.0.1:5800", true)
                    .add_header("X-Plex-Token", token, true)
                    .add_header(
                        "X-Plex-Client-Identifier",
                        "hub-cache-test",
                        true,
                    )
                    .add_header("X-Plex-Product", "Plexamp", true)
                    .add_header("Accept", "application/json", true)
                    .send(service)
                    .await;
            let body = response.take_string().await.unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            json["MediaContainer"]["Hub"][0]["title"]
                .as_str()
                .unwrap_or_else(|| {
                    panic!("hub response did not contain a title: {body}")
                })
                .to_string()
        }

        assert_eq!(hub_title(&service, "hub-account-a").await, "Account A");
        assert_eq!(hub_title(&service, "hub-account-b").await, "Account B");
        assert_eq!(hub_title(&service, "hub-account-a").await, "Account A");
        assert_eq!(
            account_a.hits(),
            1,
            "account A should reuse only its own cache entry"
        );
        assert_eq!(
            account_b.hits(),
            1,
            "account B must fetch its own authorised payload"
        );
    }

    #[tokio::test]
    async fn timeline_stop_marks_hubs_stale() {
        let _ = tracing_subscriber::fmt::try_init();
        let mock_server = get_mock_server();
        let _env = crate::test_helpers::pin_default_env(
            &mock_server.address().to_string(),
        );

        // A fresh age record must exist before the playback event.
        crate::hub_cache::track_fetched("hubcache:/hubs/promoted".to_string())
            .await;
        assert!(
            !crate::hub_cache::is_stale(
                "hubcache:/hubs/promoted",
                Duration::from_secs(300)
            )
            .await
        );

        let service = Service::new(super::route());

        // Proxied through the catch-all; the upstream 404 from the mock is
        // irrelevant, the invalidation happens on the way through.
        let _res = TestClient::post("http://127.0.0.1:5800/:/timeline?state=stopped&ratingKey=1&key=/library/metadata/1")
        .add_header("HOST", mock_server.address().to_string(), true)
        .add_header("X-Plex-Token", "fakeID", true)
        .add_header("X-Plex-Client-Identifier", "fakeID", true)
        .send(&service )
        .await;

        assert!(
            crate::hub_cache::is_stale(
                "hubcache:/hubs/promoted",
                Duration::from_secs(300)
            )
            .await,
            "timeline stop observed through the proxy must mark hubs stale"
        );
    }

    #[test]
    fn photo_cache_keys_are_account_scoped_but_token_placement_is_canonical() {
        let raw = "/photo/:/transcode?width=300&height=450&url=%2Flibrary%2Fmetadata%2F1%2Fthumb%2F123";
        let a = photo_cache_key_for(raw, Some("token-a"));
        let a_again = photo_cache_key_for(raw, Some("token-a"));
        let b = photo_cache_key_for(raw, Some("token-b"));

        assert_eq!(a, a_again);
        assert_ne!(a, b, "different accounts must not share Plex artwork");
        assert!(a.starts_with("photo:u:"));
        assert!(!a.contains("token-a"), "raw token must not appear in key");

        let with_top_level_token = format!("{raw}&X-Plex-Token=token-a");
        assert_eq!(
            a,
            photo_cache_key_for(&with_top_level_token, Some("token-a")),
            "token placement must not fragment one account's artwork cache"
        );

        assert_eq!(
            extract_inner_token(
                "/library/metadata/1/thumb/123?width=300&X-Plex-Token=nested-token&height=450"
            )
            .as_deref(),
            Some("nested-token"),
            "nested photo authentication must be recoverable for account scoping"
        );
        assert_eq!(
            strip_inner_token(
                "/library/metadata/1/thumb/123?width=300&X-Plex-Token=nested-token&height=450"
            ),
            "/library/metadata/1/thumb/123?width=300&height=450",
            "removing nested authentication must preserve later image parameters"
        );
    }

    #[tokio::test]
    async fn photo_memory_cache_cannot_cross_account_scope() {
        let raw = "/photo/:/transcode?width=300&url=%2Flibrary%2Fmetadata%2F987%2Fthumb%2F1";
        let a = photo_cache_key_for(raw, Some("photo-account-a"));
        let b = photo_cache_key_for(raw, Some("photo-account-b"));
        PHOTO_CACHE.invalidate(&a).await;
        PHOTO_CACHE.invalidate(&b).await;

        PHOTO_CACHE
            .insert(
                a.clone(),
                CachedImage {
                    content_type: Some("image/jpeg".to_string()),
                    cache_control: None,
                    body: b"account-a-image".to_vec(),
                },
            )
            .await;

        assert!(PHOTO_CACHE.get(&a).await.is_some());
        assert!(
            PHOTO_CACHE.get(&b).await.is_none(),
            "account B must not hit artwork cached by account A"
        );
        PHOTO_CACHE.invalidate(&a).await;
    }

    #[tokio::test]
    async fn web_assets_cached_with_immutable_headers() {
        let _ = tracing_subscriber::fmt::try_init();
        let server = httpmock::MockServer::start();
        let asset_mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/web/test-asset.js");
            then.status(200)
                .header("content-type", "application/javascript")
                .body("console.log('hi')");
        });
        let _env =
            crate::test_helpers::pin_default_env(&server.address().to_string());

        let service = Service::new(super::route());

        for i in 0..2 {
            let res =
                TestClient::get("http://127.0.0.1:5800/web/test-asset.js")
                    .add_header("HOST", server.address().to_string(), true)
                    .send(&service)
                    .await;
            assert_eq!(res.status_code, Some(StatusCode::OK), "request {i}");
            assert_eq!(
                res.headers().get("cache-control").unwrap(),
                "public, max-age=31536000, immutable"
            );
            assert_eq!(
                res.headers().get("content-type").unwrap(),
                "application/javascript"
            );
        }
        assert_eq!(
            asset_mock.hits(),
            1,
            "second request must be served from the in-memory cache"
        );
    }

    #[tokio::test]
    async fn test_ping() {
        let _ = tracing_subscriber::fmt::try_init();
        let mock_server = get_mock_server();
        let _env = crate::test_helpers::pin_default_env(
            mock_server.address().to_string().as_str(),
        );

        let service = Service::new(super::route());

        let content =
            TestClient::get(format!("http://127.0.0.1:5800/{}", "ping"))
                .add_header("HOST", "127.0.0.1:5800", true)
                .send(&service)
                .await
                .take_string()
                .await
                .unwrap();
        assert_eq!(content, "pong!");
    }

    #[tokio::test]
    async fn dynamic_upstream_host_is_rejected_before_credentials_can_be_forwarded(
    ) {
        let mock_server = get_mock_server();
        let _env = crate::test_helpers::pin_default_env(
            mock_server.address().to_string().as_str(),
        );
        let service = Service::new(super::route());

        let attacker = data_encoding::BASE32
            .encode(b"http://attacker.invalid")
            .to_ascii_lowercase();
        let rejected = TestClient::get("http://127.0.0.1:5800/ping")
            .add_header("HOST", format!("{attacker}.replex.stream"), true)
            .add_header("X-Plex-Token", "must-not-leak", true)
            .send(&service)
            .await;
        assert_eq!(rejected.status_code, Some(StatusCode::BAD_REQUEST));

        let configured = data_encoding::BASE32
            .encode(mock_server.base_url().as_bytes())
            .to_ascii_lowercase();
        let allowed = TestClient::get("http://127.0.0.1:5800/ping")
            .add_header("HOST", format!("{configured}.replex.stream"), true)
            .send(&service)
            .await;
        assert_eq!(allowed.status_code, Some(StatusCode::OK));
    }
}

/// Tests for the canonical library disk-cache keys and the
/// raw-payload-cache/always-transform architecture.
#[cfg(test)]
mod library_cache_tests {
    use super::*;

    /// Exactly the path the library warmer constructs in
    /// `hub_cache::warm_library_pages`.
    const WARM_PATH: &str =
        "/library/sections/6/all?X-Plex-Container-Start=0&X-Plex-Container-Size=50";

    #[test]
    fn canonical_library_path_strips_token_and_is_order_stable() {
        let with_token = canonical_library_path(&format!(
            "{}&X-Plex-Token=secret",
            WARM_PATH
        ));
        let no_token = canonical_library_path(WARM_PATH);
        let reordered = canonical_library_path(
            "/library/sections/6/all?X-Plex-Container-Size=50&X-Plex-Container-Start=0",
        );
        // What real Plex Web clients attach to a library browse: client
        // metadata plus field-shaping params. None of it changes WHICH
        // items come back, so none of it may fragment the cache.
        let with_client_noise = canonical_library_path(&format!(
            "{}&excludeFields=summary&includeGeolocation=1&X-Plex-Client-Identifier=pxweb",
            WARM_PATH
        ));

        assert_eq!(with_token, no_token, "token must not influence the key");
        assert!(
            !with_token.contains("secret"),
            "raw tokens must never appear in keys"
        );
        assert_eq!(
            no_token, reordered,
            "param order must not influence the key"
        );
        assert_eq!(
            no_token, with_client_noise,
            "client-shaping noise must not fragment the cache: warmed entries \
             must be consumable by real client requests"
        );

        // Payload-defining inputs must stay in the key.
        assert_ne!(
            canonical_library_path(WARM_PATH),
            canonical_library_path(
                "/library/sections/6/all?X-Plex-Container-Start=50&X-Plex-Container-Size=50"
            ),
            "different pages must be distinct entries"
        );
        assert_ne!(
            canonical_library_path(WARM_PATH),
            canonical_library_path(&format!("{}&type=1", WARM_PATH)),
            "filtered queries must be distinct entries"
        );
    }

    #[test]
    fn library_keys_are_user_scoped() {
        let a1 = library_cache_key_for(WARM_PATH, Some("tokenA"));
        let a2 = library_cache_key_for(WARM_PATH, Some("tokenA"));
        let b = library_cache_key_for(WARM_PATH, Some("tokenB"));

        assert_eq!(a1, a2, "same account must hit the same entry");
        assert_ne!(
            a1, b,
            "different accounts must never share library cache entries: raw \
             library payloads embed per-account watch state"
        );
        assert!(
            a1.starts_with("library:u:"),
            "library keys are always user-scoped"
        );
        assert!(!a1.contains("tokenA"), "raw tokens must not appear in keys");
    }

    /// The review's key finding: warmer and request path must derive
    /// identical keys from the same request, or warmed entries land where
    /// nothing looks them up.
    #[test]
    fn library_warmer_key_matches_request_path() {
        let warmer_key = library_cache_key_for(WARM_PATH, Some("admintoken"));

        // The same request as a real Plex Web client sends it: token in the
        // header plus the usual client-metadata and field-shaping noise.
        const CLIENT_NOISE: &str = "excludeFields=summary&includeGeolocation=1\
            &X-Plex-Client-Identifier=pxweb&X-Plex-Platform=Safari";
        let mut probe = salvo::http::Request::default();
        let uri: hyper::Uri =
            format!("{}&{}&X-Plex-Token=admintoken", WARM_PATH, CLIENT_NOISE)
                .parse()
                .unwrap();
        probe.set_uri(uri);
        probe.headers_mut().insert(
            "X-Plex-Token",
            salvo::http::header::HeaderValue::from_static("admintoken"),
        );
        assert_eq!(
            library_cache_key(&probe),
            warmer_key,
            "warmed entries must be consumable by real client requests, \
             noise and all"
        );

        // Token sent as a query param instead of a header must produce the
        // same key (canonical path strips it; scope comes from the value).
        let mut probe_query_token = salvo::http::Request::default();
        probe_query_token.set_uri(
            format!("{}&{}&X-Plex-Token=admintoken", WARM_PATH, CLIENT_NOISE)
                .parse()
                .unwrap(),
        );
        assert_eq!(library_cache_key(&probe_query_token), warmer_key);

        // A different account must not consume the warmed entry.
        let mut probe_other = salvo::http::Request::default();
        probe_other.set_uri(
            format!("{}&X-Plex-Token=othertoken", WARM_PATH)
                .parse()
                .unwrap(),
        );
        assert_ne!(
            library_cache_key(&probe_other),
            warmer_key,
            "one account must never read another account's library payload"
        );
    }

    /// Cache misses must fetch without field-shaping params so every stored
    /// raw payload is a full-field superset — but the auth token and paging
    /// must survive normalization untouched.
    #[test]
    fn normalize_library_fetch_preserves_auth_and_paging() {
        let mut req = salvo::http::Request::default();
        let raw = format!(
            "{}&excludeFields=summary&includeGeolocation=1\
             &X-Plex-Container-Size=50&X-Plex-Container-Start=0&X-Plex-Token=tok",
            WARM_PATH
        );
        req.set_uri(raw.parse().unwrap());
        normalize_library_fetch(&mut req);

        assert_eq!(req.uri().path(), "/library/sections/6/all");
        let query = req.uri().query().unwrap_or_default();
        assert!(
            query.contains("X-Plex-Token=tok"),
            "auth token must survive normalization: {query}"
        );
        assert!(
            query.contains("X-Plex-Container-Size=50")
                && query.contains("X-Plex-Container-Start=0"),
            "paging must survive normalization: {query}"
        );
        assert!(
            !query.contains("excludeFields") && !query.contains("includeGeolocation"),
            "field shaping must be dropped so stored payloads are supersets: {query}"
        );
    }

    /// The review's "Monday 4K / Tuesday 1080p" scenario: a policy change
    /// must be honoured on disk-cache hits. The stored body is the RAW
    /// upstream payload, so the response must change when the policy does —
    /// a cached entry is never itself an authorisation decision.
    #[tokio::test]
    async fn policy_change_is_honoured_on_library_disk_cache_hits() {
        use crate::test_helpers::*;
        use salvo::test::{ResponseExt, TestClient};

        let _ = tracing_subscriber::fmt::try_init();
        let server = httpmock::MockServer::start();

        let _identity_mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v2/user")
                .header("X-Plex-Token", "fakeID");
            then.status(200)
                .header("content-type", "application/json")
                .body_from_file("tests/mock/in/identity_user.json");
        });
        let sections_mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/library/sections/6/all")
                .header("X-Plex-Token", "fakeID");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 1,
                            "Metadata": [{
                                "ratingKey": "100",
                                "key": "/library/metadata/100",
                                "title": "4K Test Movie",
                                "type": "movie",
                                "Media": [{
                                    "id": 1,
                                    "videoResolution": "4k",
                                    "width": 3840,
                                    "height": 2160,
                                    "Part": [{
                                        "id": 101,
                                        "key": "/library/parts/100/101/file.mkv"
                                    }]
                                }]
                            }]
                        }
                    }"#,
                );
        });

        // Isolate the disk cache for this test.
        let tmp = std::env::temp_dir().join(format!(
            "replex-test-libcache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let _ = std::fs::remove_dir_all(&tmp);

        let _env = pin_default_env(&server.address().to_string());
        std::env::set_var("REPLEX_IDENTITY_API_BASE", server.base_url());
        std::env::set_var("REPLEX_RESOLUTION_POLICY_ENABLED", "true");
        std::env::set_var("REPLEX_RESOLUTION_POLICY_FAIL_CLOSED", "true");
        std::env::set_var("REPLEX_RESOLUTION_DEFAULT", "unlimited");
        std::env::set_var("REPLEX_DISK_CACHE_DIR", &tmp);

        let mut service = Service::new(super::route());
        // Realistic client URL: the field-shaping/client-metadata noise a
        // real Plex Web session attaches must not prevent disk-cache reuse.
        let url = "http://127.0.0.1:5800/library/sections/6/all?excludeFields=summary&includeGeolocation=1&X-Plex-Client-Identifier=testclient&X-Plex-Container-Size=50&X-Plex-Container-Start=0";

        // Monday: unrestricted account, 4K version is visible.
        let content_monday = TestClient::get(url)
            .add_header("HOST", server.address().to_string(), true)
            .add_header("X-Plex-Token", "fakeID", true)
            .add_header("X-Plex-Client-Identifier", "fakeID", true)
            .add_header("Accept", "application/json", true)
            .send(&service)
            .await
            .take_string()
            .await
            .unwrap();
        let monday: serde_json::Value =
            serde_json::from_str(&content_monday).unwrap();
        assert_eq!(
            sections_mock.hits(),
            1,
            "first request must fetch upstream"
        );
        assert!(
            monday["MediaContainer"]["Metadata"]
                .as_array()
                .map(|m| !m.is_empty())
                .unwrap_or(false),
            "unrestricted account must see the item: {monday}"
        );
        assert!(
            monday["MediaContainer"]["Metadata"][0]["Media"]
                .as_array()
                .map(|m| !m.is_empty())
                .unwrap_or(false),
            "unrestricted account must see the 4K media version"
        );

        // Tuesday: the policy tightens to 1080p. Same URL, same token —
        // the response must be served from the disk cache (no second
        // upstream fetch) AND reflect the new policy. The value is quoted
        // so figment keeps it a string (deserialize_resolution_default
        // rejects bare integers from env).
        std::env::set_var("REPLEX_RESOLUTION_DEFAULT", "\"1080\"");
        service = Service::new(super::route());

        let content_tuesday = TestClient::get(url)
            .add_header("HOST", server.address().to_string(), true)
            .add_header("X-Plex-Token", "fakeID", true)
            .add_header("X-Plex-Client-Identifier", "fakeID", true)
            .add_header("Accept", "application/json", true)
            .send(&service)
            .await
            .take_string()
            .await
            .unwrap();
        let tuesday: serde_json::Value =
            serde_json::from_str(&content_tuesday).unwrap();

        assert_eq!(
            sections_mock.hits(),
            1,
            "second request must be served from the disk cache, not upstream"
        );
        assert_ne!(
            monday, tuesday,
            "a policy change must change the served response even on a cache hit"
        );
        let hidden = tuesday["MediaContainer"]
            .get("Metadata")
            .map(|m| m.as_array().map(|a| a.is_empty()).unwrap_or(true))
            .unwrap_or(true);
        assert!(
            hidden,
            "restricted account must not see the 4K item: {tuesday}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The review's explicit P1 ask: an end-to-end cross-user test with one
    /// unlimited account and one 1080p account. Asserts through the real
    /// router that (a) the restricted account never sees the 4K item while
    /// the unlimited account does, (b) the accounts never share disk-cache
    /// entries (each fetches its own raw payload), and (c) the restricted
    /// account's cache hits still re-apply its policy.
    #[tokio::test]
    async fn cross_user_library_sections_isolation() {
        use crate::test_helpers::*;
        use salvo::test::{ResponseExt, TestClient};

        let _ = tracing_subscriber::fmt::try_init();
        let server = httpmock::MockServer::start();

        // Two verified identities, one per token.
        let _admin_identity = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v2/user")
                .header("X-Plex-Token", "4ktoken");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"id": 1, "uuid": "uuid-admin", "username": "admin"}"#,
                );
        });
        let _limited_identity = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v2/user")
                .header("X-Plex-Token", "limitedtoken");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"id": 2, "uuid": "uuid-limited", "username": "jodiemy3"}"#);
        });
        let sections_mock = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/library/sections/6/all");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 1,
                            "Metadata": [{
                                "ratingKey": "100",
                                "key": "/library/metadata/100",
                                "title": "4K Test Movie",
                                "type": "movie",
                                "Media": [{
                                    "id": 1,
                                    "videoResolution": "4k",
                                    "width": 3840,
                                    "height": 2160,
                                    "Part": [{
                                        "id": 101,
                                        "key": "/library/parts/100/101/file.mkv"
                                    }]
                                }]
                            }]
                        }
                    }"#,
                );
        });

        let tmp = std::env::temp_dir().join(format!(
            "replex-test-crossuser-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let _ = std::fs::remove_dir_all(&tmp);

        let _env = pin_default_env(&server.address().to_string());
        std::env::set_var("REPLEX_IDENTITY_API_BASE", server.base_url());
        std::env::set_var("REPLEX_RESOLUTION_POLICY_ENABLED", "true");
        std::env::set_var("REPLEX_RESOLUTION_POLICY_FAIL_CLOSED", "true");
        std::env::set_var("REPLEX_RESOLUTION_DEFAULT", "unlimited");
        std::env::set_var(
            "REPLEX_USER_RESOLUTION_POLICIES",
            r#"[{"username": "jodiemy3", "max_resolution": "1080"}]"#,
        );
        std::env::set_var("REPLEX_DISK_CACHE_DIR", &tmp);

        let service = Service::new(super::route());
        let url = "http://127.0.0.1:5800/library/sections/6/all?X-Plex-Container-Size=50&X-Plex-Container-Start=0";

        async fn browse(
            service: &Service,
            url: &str,
            token: &str,
        ) -> serde_json::Value {
            let content = TestClient::get(url)
                .add_header("HOST", "127.0.0.1:5800", true)
                .add_header("X-Plex-Token", token, true)
                .add_header("X-Plex-Client-Identifier", "test-client", true)
                .add_header("Accept", "application/json", true)
                .send(service)
                .await
                .take_string()
                .await
                .unwrap();
            serde_json::from_str(&content).unwrap()
        }
        let metadata = |v: &serde_json::Value| {
            v["MediaContainer"]
                .get("Metadata")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default()
        };

        // Unlimited account: sees the item with its 4K media version.
        let admin = browse(&service, url, "4ktoken").await;
        assert_eq!(
            sections_mock.hits(),
            1,
            "admin request must fetch upstream"
        );
        assert_eq!(
            metadata(&admin).len(),
            1,
            "unlimited account must see the item: {admin}"
        );
        assert!(
            !metadata(&admin)[0]["Media"]
                .as_array()
                .unwrap_or(&vec![])
                .is_empty(),
            "unlimited account must see the 4K media version"
        );

        // 1080p account: same URL — but must NOT see the 4K item, and must
        // fetch its OWN raw payload rather than reading the admin's entry.
        let limited = browse(&service, url, "limitedtoken").await;
        assert_eq!(
            sections_mock.hits(),
            2,
            "restricted account must use its own user-scoped cache scope, \
             never the admin's entry"
        );
        assert!(
            metadata(&limited).is_empty(),
            "1080p account must never see the 4K item: {limited}"
        );

        // 1080p account again: served from its own disk entry (no new
        // upstream fetch) and the restriction is still applied.
        let limited_again = browse(&service, url, "limitedtoken").await;
        assert_eq!(
            sections_mock.hits(),
            2,
            "restricted account's repeat request must be a disk-cache hit"
        );
        assert!(
            metadata(&limited_again).is_empty(),
            "restriction must survive the restricted account's cache hit: {limited_again}"
        );

        // Unlimited account again: served from its own disk entry, still
        // sees the full 4K item — the restricted policy never leaked into
        // the admin's cache scope.
        let admin_again = browse(&service, url, "4ktoken").await;
        assert_eq!(
            sections_mock.hits(),
            2,
            "unlimited account's repeat request must be a disk-cache hit"
        );
        assert_eq!(
            metadata(&admin_again).len(),
            1,
            "admin's cache scope must be unaffected by the restricted policy: {admin_again}"
        );
        assert!(
            !metadata(&admin_again)[0]["Media"]
                .as_array()
                .unwrap_or(&vec![])
                .is_empty(),
            "admin must still see the 4K media version on cache hits"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
