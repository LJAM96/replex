use crate::config::Config;
use crate::logging::*;
use crate::models::*;
use crate::plex_client::*;
use crate::resolution_policy::{
    allowed_media, best_allowed_media, media_allowed, resolve_policy,
};
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
use tokio::time::Duration;
use url::Url;

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
    let config: Config = Config::figment().extract().unwrap();

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
    let mut router = Router::with_hoop(cors)
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
/// upstream work on first view. Posters are stable per item+size, so cache
/// them (per unique query minus the client token) and let every user share
/// the warm entries.
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
    canonical_photo_key(&raw)
}

/// Canonical key for a photo request path+query. Strips the per-user
/// token from BOTH the top-level query and inside the nested `url=`
/// parameter (Plex Web appends its own token there), so every account
/// shares one cache entry per unique image.
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
    match v.find_once_token() {
        Some(idx) => v[..idx].trim_end_matches('?').to_string(),
        None => v.to_string(),
    }
}

trait FindOnceToken {
    fn find_once_token(&self) -> Option<usize>;
}
impl FindOnceToken for str {
    fn find_once_token(&self) -> Option<usize> {
        let lower = self.to_ascii_lowercase();
        ["x-plex-token=", "&x-plex-token="]
            .iter()
            .filter_map(|needle| lower.find(needle).map(|i| (i, needle.len())))
            .min_by(|a, b| a.0.cmp(&b.0))
            .map(|(i, _)| i)
    }
}

/// Short hash of a token used as the user-scope component of library cache
/// keys. Raw tokens never appear in cache keys.
fn library_user_scope(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    data_encoding::HEXLOWER.encode(&digest)[..16].to_string()
}

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
        .map(library_user_scope)
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
        match apply_policy_transforms(req, res).await {
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
    let key = photo_cache_key(req);
    if let Some(record) = crate::disk_cache::get_full(&key).await {
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
    let proxy = default_proxy();
    proxy.handle(req, depot, res, ctrl).await;
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
    let proxy = default_proxy();
    let headers_ori = req.headers().clone();
    req.headers_mut().insert(
        http::header::ACCEPT,
        header::HeaderValue::from_static("application/json"),
    );
    proxy.handle(req, depot, res, ctrl).await;
    *req.headers_mut() = headers_ori;
    Ok(())
}

// skip processing when product is plexamp
#[handler]
async fn should_skip(
    req: &mut Request,
    res: &mut Response,
    depot: &mut Depot,
    ctrl: &mut FlowCtrl,
) {
    let context: PlexContext = req.extract().await.unwrap();

    let is_livetv = match context.path.clone() {
        Some(v) => v.contains("livetv"),
        None => false,
    };

    let is_plexamp = match context.product.clone() {
        Some(v) => v.to_lowercase().contains("plexamp"),
        None => false,
    };

    if is_livetv || is_plexamp {
        let config: Config = Config::dynamic(req).extract().unwrap();
        let proxy = default_proxy();

        proxy.handle(req, depot, res, ctrl).await;
        ctrl.skip_rest();
    }
}

#[handler]
async fn redirect_stream(
    req: &mut Request,
    _depot: &mut Depot,
    res: &mut Response,
) {
    perform_stream_redirect(req, res).await
}

async fn perform_stream_redirect(req: &mut Request, res: &mut Response) {
    let config: Config = Config::dynamic(req).extract().unwrap();
    let redirect_url = if config.redirect_streams_host.clone().is_some() {
        format!(
            "{}{}",
            config.redirect_streams_host.clone().unwrap(),
            req.uri_mut().path_and_query().unwrap()
        )
    } else {
        format!(
            "{}{}",
            config.host.unwrap(),
            req.uri_mut().path_and_query().unwrap()
        )
    };
    let mime = mime_guess::from_path(req.uri().path()).first_or_octet_stream();
    res.headers_mut()
        .insert(CONTENT_TYPE, mime.as_ref().parse().unwrap());
    res.render(Redirect::temporary(redirect_url));
}

/// Proxy a stream through Replex instead of 302-redirecting the client to the
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

/// Stream gating by the resolution policy.
///
/// Order matters: authenticate, apply policy, and only then serve the bytes.
/// - Policy disabled: legacy behaviour (plain 302 redirect to the origin).
/// - Unrestricted account: plain 302 redirect (no limit applies).
/// - Restricted account: the bytes are proxied THROUGH Replex, never handed
///   the Plex origin URL, so the limit stays enforceable. `/library/parts`
///   requests are checked against the part policy cache; prohibited parts get
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
    let config: Config = Config::dynamic(req).extract().unwrap();
    if !config.resolution_policy_enabled {
        perform_stream_redirect(req, res).await;
        return;
    }

    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            res.status_code(StatusCode::UNAUTHORIZED);
            return;
        }
    };

    let identity = match plex_client.get_current_user().await {
        Ok(identity) => identity,
        Err(error) => {
            if config.resolution_policy_fail_closed {
                tracing::warn!(
                    error = %error,
                    uri = ?req.uri(),
                    "Identity unavailable, failing closed on stream request"
                );
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                return;
            }
            tracing::warn!(
                error = %error,
                uri = ?req.uri(),
                "Identity unavailable, failing open on stream request"
            );
            perform_stream_redirect(req, res).await;
            return;
        }
    };

    let policy = resolve_policy(
        &config.user_resolution_policies,
        config.resolution_default,
        config.hidden_collections.as_deref().unwrap_or(&[]),
        &identity,
    );
    if policy.is_unrestricted() {
        // Unlimited accounts keep the direct-origin redirect for performance;
        // no resolution limit applies to them.
        perform_stream_redirect(req, res).await;
        return;
    }

    // Restricted account: the stream MUST stay behind Replex so the policy
    // decision is enforced and the client never learns the Plex origin URL. A
    // direct 302 to the origin would let the client bypass the limit entirely.
    if let Some(part_id) = req.param::<i64>("partid") {
        let part_key = crate::plex_client::part_policy_key(
            &identity.uuid,
            &policy,
            part_id,
        );
        match crate::plex_client::PART_POLICY_CACHE.get(&part_key).await {
            Some(true) => {
                tracing::debug!(
                    username = %identity.username,
                    part_id = part_id,
                    "Permitted part requested; proxying through Replex"
                );
                proxy_stream_through_replex(req, depot, res, ctrl).await;
            }
            Some(false) => {
                tracing::info!(
                    username = %identity.username,
                    part_id = part_id,
                    maximum = ?policy.limit,
                    "Blocked direct access to prohibited part"
                );
                res.status_code(StatusCode::FORBIDDEN);
            }
            None => {
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
    proxy_stream_through_replex(req, depot, res, ctrl).await
}

// Google tv requests some weird thumbnail for hero elements. Let fix that
#[handler]
async fn fix_photo_transcode_request(
    req: &mut Request,
    _depot: &mut Depot,
    res: &mut Response,
) {
    let context: PlexContext = req.extract().await.unwrap();
    if context.size.is_some() && context.clone().size.unwrap().contains('-')
    // (catched things like (medlium-240, large-500),i dont think size paramater orks at all, but who knows
    // && context.platform.is_some()
    // && context.clone().platform.unwrap().to_lowercase() == "android"
    {
        let size: String = context
            .clone()
            .size
            .unwrap()
            .split('-')
            .last()
            .unwrap()
            .parse()
            .unwrap();
        add_query_param_salvo(req, "height".to_string(), size.clone());
        add_query_param_salvo(req, "width".to_string(), size.clone());
        //add_query_param_salvo(req, "quality".to_string(), "80".to_string());
    }
}

// resolve a local media path to full url
#[handler]
async fn resolve_local_media_path(req: &mut Request, res: &mut Response) {
    let mut context: PlexContext = req.extract().await.unwrap();
    let url = req.query::<String>("url");
    if url.is_some() && url.clone().unwrap().contains("/replex/image/hero") {
        let uri: url::Url = url::Url::parse(url.unwrap().as_str()).unwrap();
        let segments = uri.path_segments().unwrap().collect::<Vec<&str>>();

        let uuid = segments.last().unwrap().replace(".jpg", "");
        //if context.token.is_none() {
        //    context.token = Some(segments.last().unwrap().to_string());
        //}

        let plex_client = match PlexClient::from_context(&context) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "failed to build Plex client from request context");
                res.status_code(StatusCode::UNAUTHORIZED);
                return;
            }
        };
        let rurl = plex_client.get_hero_art(uuid.to_string()).await;
        if rurl.is_some() {
            add_query_param_salvo(req, "url".to_string(), rurl.unwrap());
        }
    }
}

#[handler]
async fn disable_related_query(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    add_query_param_salvo(req, "includeRelated".to_string(), "0".to_string());
}

#[handler]
async fn debug(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    //dbg!("tequested");
    let context: PlexContext = req.extract().await.unwrap();
    dbg!(&context.token);
    //dbg!(&req);
}

#[handler]
async fn ntf_watchlist_force(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    // use memory_stats::memory_stats;
    // dbg!(memory_stats().unwrap().physical_mem / 1024 / 1000);
    let context: PlexContext = req.extract().await.unwrap();
    if context.clone().token.is_some() {
        tokio::spawn(async move {
            let token = context.clone().token.unwrap();
            let client_id = context.clone().client_identifier.unwrap();
            let url = format!("https://notifications.plex.tv/api/v1/notifications/settings?X-Plex-Token={}", &token);
            let json_data = r#"{"enabled": true,"libraries": [],"identifier": "tv.plex.notification.library.new"}"#;
            let client = reqwest::Client::new();

            tracing::info!(
                username = %context.clone().username.unwrap_or_default(),
                platform = %context.clone().product.unwrap_or_default(),
                platform = %context.clone().device_name.unwrap_or_default(),
                "Bootstrao for request"
            );

            let client_base = "https://clients.plex.tv";
            let res = client
                .get(format!("{}/api/v2/user", client_base))
                .header("Accept", "application/json")
                .header("X-Plex-Token", &token)
                .header("X-Plex-Client-Identifier", &client_id)
                .send()
                .await
                .unwrap();

            if !res.status().is_success() {
                tracing::info!("cannot get user");
                return;
            }

            let user: PlexUser = res.json().await.unwrap();
            tracing::info!(
                id = %user.id,
                uuid = %user.uuid,
                username = %user.username,
                "got user"
            );

            let response = client
                .post(url)
                .header("Content-Type", "application/json")
                .body(json_data.to_owned())
                .send()
                .await
                .unwrap();

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
            for key in opts {
                let response = client
                    .post(format!("{}?key={}&value=opt_out", u.clone(), key))
                    .header("Accept", "application/json")
                    .header("X-Plex-Token", &token)
                    .header("X-Plex-Client-Identifier", &client_id)
                    .send()
                    .await
                    .unwrap();

                tracing::info!(
                status = %response.status(),
                "opt out status"
                );
            }
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
        MediaContainerWrapper::default();
    container.content_type = content_type.clone();
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
    ctrl: &mut FlowCtrl,
    depot: &mut Depot,
) {
    let context: PlexContext = req.extract().await.unwrap();
    let t = req.param::<String>("type").unwrap();
    let uuid = req.param::<String>("uuid").unwrap();

    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            res.status_code(StatusCode::UNAUTHORIZED);
            return;
        }
    };
    let url = plex_client.get_hero_art(uuid).await;
    if url.is_none() {
        res.status_code(StatusCode::NOT_FOUND);
        return;
    }
    // let uri = url.unwrap().parse::<http::Uri>().unwrap();;
    // req.set_uri(uri);
    // let proxy = proxy("https://metadata-static.plex.tv".to_string());
    // proxy.handle(req, depot, res, ctrl).await;

    res.render(Redirect::found(url.unwrap()));
}

// if directplay fails we remove it.
#[handler]
pub async fn direct_stream_fallback(
    req: &mut Request,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
    depot: &mut Depot,
) -> Result<(), anyhow::Error> {
    let config: Config = Config::dynamic(req).extract().unwrap();
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let queries = req.queries().clone();

    let direct_play = queries
        .get("directPlay")
        .unwrap_or(&"1".to_string())
        .to_owned();

    if direct_play != "1" {
        return Ok(());
    }

    let mut res_upstream = &mut Response::new();
    proxy_for_transform
        .handle(req, depot, res_upstream, ctrl)
        .await;

    match res_upstream.status_code.unwrap() {
        http::StatusCode::OK => {
            let container: MediaContainerWrapper<MediaContainer> =
            //from_reqwest_response(upstream_res).await?;
            from_salvo_response(res_upstream).await?;

            if container.media_container.general_decision_code.is_some()
                && container.media_container.general_decision_code.unwrap()
                    == 2000
            {
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
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    // Only successful metadata payloads are worth parsing.
    let status = res.status_code.unwrap_or(StatusCode::OK);
    if status != StatusCode::OK {
        return Ok(());
    }
    apply_policy_transforms(req, res).await
}

/// Parse the RAW upstream JSON body currently held in `res` and apply the
/// requesting account's CURRENT policy transforms over it, then render the
/// result. Shared by the live path (`transform_policy_response`) and the
/// disk-cache hit path (`library_cache_lookup`) so both always transform
/// with the current account and current configuration — a cached body is
/// never served as an authorisation decision.
async fn apply_policy_transforms(
    req: &mut Request,
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
                    uri = ?req.uri(),
                    "Policy transform could not parse response"
                );
                return Err(salvo::http::StatusError::bad_request().into());
            }
        };
    container.content_type = content_type;

    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
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

/// Hub responses fetched through Replex's shared response cache.
///
/// Upstream hub payloads are identical for every user BEFORE per-user
/// transforms run (restrictions, visibility, resolution filtering all apply
/// downstream), so the raw fetch is shared across accounts. The cache key is
/// canonical: only the directory-selection and paging params define the
/// payload identity. Everything else clients attach (counts, field excludes,
/// X-Plex-* metadata) varies per client shape; folding those into the key
/// would fragment the shared cache so every client shape pays its own slow
/// cold fetch. Per-request shaping happens after the transforms instead.
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
) -> Result<(), anyhow::Error> {
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());

    let key_path = cache_key_for_request(req);
    let fetch_path = canonical_fetch_path(req, &key_path);
    let config: Config = Config::dynamic(req).extract().unwrap();

    // Shared raw-response cache for genuinely global discovery rows, keyed
    // WITHOUT the client token so the warmer (admin token) and every user
    // read/write the same entries. Account-specific hubs (Continue Watching,
    // On Deck, home/promoted rows with personal membership) get a
    // token-hash-scoped key so one account can never be served another
    // account's payload. Per-user transforms still run below on every
    // request either way.
    let cache_key =
        crate::hub_cache::hub_cache_key(&key_path, context.token.as_deref());
    let mut container: MediaContainerWrapper<MediaContainer> =
        match plex_client.cache.get(&cache_key).await {
            Some(mut cached) => {
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

    tracing::debug!(
        uri = %req.uri(),
        path = %key_path,
        hubs_before,
        hubs_after = container.media_container.children().len(),
        "hub payload transformed"
    );

    shape_canonical_hubs(&mut container, &context);

    res.render(container);
    Ok(())
}

#[handler]
pub async fn transform_hubs_response(
    req: &mut Request,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
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
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let config: Config = Config::dynamic(req).extract().unwrap();

    // Interleave disabled: pass requests through untouched so clients get
    // native per-library hub responses.
    if !config.interleave {
        return;
    }

    let config: Config = Config::dynamic(req).extract().unwrap();
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
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
            Some(c) => c[0] == pinned[0],
            // Absent → treat as first directory.
            None => true,
        };

        tracing::debug!(
            uri = %req.uri(),
            is_first,
            mobile_client,
            "promoted slot decision"
        );

        if !is_first && !mobile_client {
            // Not the first directory: return empty container so only the
            // first slot triggers a full merged fetch.
            tracing::debug!(
                uri = %req.uri(),
                "promoted non-first slot, serving intentional empty"
            );
            let mut container: MediaContainerWrapper<MediaContainer> =
                MediaContainerWrapper::default();
            container.content_type = content_type.clone();
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
    res: &mut Response,
) {
    add_query_param_salvo(req, "includeGuids".to_string(), "1".to_string());
}

// some androids have trouble loading more for hero style. So load more at once
#[handler]
pub async fn transform_req_android(req: &mut Request, res: &mut Response) {
    let config: Config = Config::dynamic(req).extract().unwrap();
    let context: PlexContext = req.extract().await.unwrap();

    let mut count = context.clone().count.unwrap_or(25);
    match context.platform.unwrap_or_default() {
        Platform::Android => count = 50,
        _ => (),
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
    _depot: &mut Depot,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let config: Config = Config::dynamic(req).extract().unwrap();
    let context: PlexContext = req.extract().await.unwrap();
    let collection_ids = req.param::<String>("ids").unwrap();
    let collection_ids: Vec<u32> = collection_ids
        .split(',')
        .filter(|&v| !v.parse::<u32>().is_err())
        .map(|v| v.parse().unwrap())
        .collect();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());

    // We dont listen to pagination. We have a hard max of 250 per collection
    let mut limit: i32 = 250;
    let mut offset: i32 = 0;

    // in we dont remove watched then we dont need to limit
    if !config.exclude_watched {
        limit = context.container_size.unwrap_or(50);
        offset = context.container_start.unwrap_or(0);
    }

    // create a stub
    let mut container: MediaContainerWrapper<MediaContainer> =
        MediaContainerWrapper::default();
    container.content_type = content_type;
    let size = container.media_container.children().len();
    container.media_container.size = Some(size.try_into().unwrap());
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
    _depot: &mut Depot,
    res: &mut Response,
) -> Result<(), anyhow::Error> {
    let config: Config = Config::dynamic(req).extract().unwrap();
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let content_type = get_content_type_from_headers(req.headers_mut());
    let style = req.param::<Style>("style").unwrap();
    let rest_path = req.param::<String>("**rest").unwrap();

    // We dont listen to pagination. We have a hard max of 250 per collection
    let mut limit: i32 = 250;
    let mut offset: i32 = 0;

    // in we dont remove watched then we dont need to limit
    if !config.exclude_watched {
        limit = context.container_size.unwrap_or(50);
        offset = context.container_start.unwrap_or(0);
    }

    let mut url = Url::parse(req.uri_mut().to_string().as_str()).unwrap();
    url.set_path(&rest_path);
    req.set_uri(hyper::Uri::try_from(url.as_str()).unwrap());

    // patch, plex seems to pass wrong contentdirid, probaply cause we all load it inti the first
    let mut queries = req.queries().clone();
    queries.remove("contentDirectoryID");
    replace_query(queries, req);

    let upstream_res = plex_client.request(req).await?;
    match upstream_res.status() {
        reqwest::StatusCode::OK => (),
        status => {
            tracing::error!(status = ?status, res = ?upstream_res, req = ?req, "Failed to get plex response");
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
        .with_transform(MediaStyleTransform { style: style })
        .with_transform(UserStateTransform)
        .with_transform(HubWatchedTransform)
        .with_transform(HubKeyTransform)
        .apply_to(&mut container)
        .await;

    res.render(container);
    Ok(())
}

#[handler]
pub async fn get_library_item_metadata(req: &mut Request, res: &mut Response) {
    let config: Config = Config::dynamic(req).extract().unwrap();
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
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

    let upstream_res = plex_client.request(req).await.unwrap();
    let mut container: MediaContainerWrapper<MediaContainer> =
        match from_reqwest_response(upstream_res).await {
            Ok(r) => r,
            Err(error) => {
                tracing::error!(error = ?error, uri = ?req.uri(), "Failed to get plex response");
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
async fn force_maximum_quality(req: &mut Request) -> Result<(), anyhow::Error> {
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let config: Config = Config::dynamic(req).extract().unwrap();
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
        queries.insert(
            "mediaBufferSize".to_string(),
            (size[0].parse::<f32>().unwrap() as i64).to_string(),
        );
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
    if queries.contains_key(&query_key) {
        let extra = &queries.remove(&query_key.clone()).unwrap()[0];

        let filtered_extra = extra
            .split("+")
            .filter(|s| {
                !s.contains("add-limitation")
                    && !s.to_lowercase().contains("name=video.bitrate")
            })
            .join("+");

        queries.insert(query_key, filtered_extra);
    };

    if config.force_direct_play_for.is_some() && queries.get("path").is_some() {
        let resos = config.force_direct_play_for.unwrap();
        let item = plex_client
            .clone()
            .get_item_by_key(req.queries().get("path").unwrap().to_string())
            .await
            .unwrap();

        let media_index: usize = if req.queries().get("mediaIndex").is_none()
            || req.queries().get("mediaIndex").unwrap() == "-1"
        {
            0
        } else {
            req.queries()
                .get("mediaIndex")
                .unwrap()
                .parse::<usize>()
                .unwrap()
        };

        let media_item =
            item.media_container.metadata[0].media[media_index].clone();

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
//     let plex_client = PlexClient::from_context(&context);
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
) -> Result<TranscodingStatus, anyhow::Error> {
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };

    let mut res = &mut Response::new();
    let mut depot = &mut Depot::new();
    let mut ctrl = &mut FlowCtrl::new(vec![]);
    proxy_for_transform.handle(req, depot, res, ctrl).await;
    dbg!(&res);

    let ress = plex_client.proxy_request(&req).await?;
    dbg!(&ress);
    //dbg!(&req);

    let transcode: MediaContainerWrapper<MediaContainer> =
        from_salvo_response(res).await?;
    let mut is_transcoding = false;

    if transcode.media_container.size.is_some()
        && transcode.media_container.size.unwrap() == 0
    {
        return Ok(TranscodingStatus {
            is_transcoding,
            decision_result: transcode,
        });
    }

    let streams =
        &transcode.media_container.metadata[0].media[0].parts[0].streams;
    // let selected_media = transcode.media_container.metadata[0].media[0].clone();
    for stream in streams {
        if stream.stream_type.clone().unwrap() == 1
            && stream.decision.clone().unwrap_or("unknown".to_string())
                == "transcode"
        {
            is_transcoding = true;
            break;
        }
    }

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
    res: &mut salvo::Response,
    ctrl: &mut FlowCtrl,
) -> Result<(), anyhow::Error> {
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };
    let config: Config = Config::dynamic(req).extract().unwrap();
    let mut queries = req.queries().clone();
    let original_queries = req.queries().clone();
    let media_index: usize = if req.queries().get("mediaIndex").is_none()
        || req.queries().get("mediaIndex").unwrap() == "-1"
    {
        0
    } else {
        req.queries()
            .get("mediaIndex")
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };

    let fallback_for =
        config.video_transcode_fallback_for.unwrap()[0].to_lowercase();

    let item = plex_client
        .clone()
        .get_item_by_key(req.queries().get("path").unwrap().to_string())
        .await
        .unwrap();

    if item.media_container.metadata[0].media[media_index]
        .video_resolution
        .clone()
        .unwrap()
        .to_lowercase()
        != fallback_for
    {
        tracing::debug!("Media item not marked for fallback, continue playing");
        return Ok(());
    }

    if item.media_container.metadata[0].media.len() <= 1 {
        tracing::debug!("Nothing to fallback on, skipping fallback check");
    } else {
        // execute_video_transcode_fallback(req, item, media_index).await?;
        // let response = plex_client.request(req).await?;
        // let mut transcode: MediaContainerWrapper<MediaContainer> =
        //     from_reqwest_response(response).await?;
        // let streams =
        //     &transcode.media_container.metadata[0].media[0].parts[0].streams;
        // let selected_media =
        //     transcode.media_container.metadata[0].media[0].clone();
        let mut requested_bitrate: Option<i64> = None;
        if queries.get("videoBitrate").is_some() {
            requested_bitrate =
                Some(queries.get("videoBitrate").unwrap().parse().unwrap());
        } else if queries.get("maxVideoBitrate").is_some() {
            requested_bitrate =
                Some(queries.get("maxVideoBitrate").unwrap().parse().unwrap());
        }

        let mut fallback_selected = false;
        // this could fail.
        let status: TranscodingStatus =
            get_transcoding_for_request(req).await?;
        let selected_media = item.media_container.metadata[0].media[0].clone();
        let mut available_media_ids: Vec<i64> = item.media_container.metadata
            [0]
        .media
        .iter()
        .map(|x| x.id)
        .collect();
        available_media_ids.retain(|x| *x != selected_media.id);
        // available_media_ids.remove(selected_media.id);
        if status.is_transcoding {
            tracing::debug!(
                "{} transcoding, looking for fallback",
                selected_media
            );

            let mut media_items =
                item.media_container.metadata[0].media.clone();

            // Policy: a restricted account must never fall back across its
            // limit, so candidates are filtered to permitted versions first.
            if config.resolution_policy_enabled {
                match plex_client.get_current_user().await {
                    Ok(identity) => {
                        let policy = resolve_policy(
                            &config.user_resolution_policies,
                            config.resolution_default,
                            config.hidden_collections.as_deref().unwrap_or(&[]),
                            &identity,
                        );
                        if !policy.is_unrestricted() {
                            let before = media_items.len();
                            media_items = allowed_media(&media_items, &policy);
                            tracing::debug!(
                                username = %identity.username,
                                before = before,
                                after = media_items.len(),
                                "Transcode fallback candidates filtered by resolution policy"
                            );
                        }
                    }
                    Err(error) => {
                        if config.resolution_policy_fail_closed {
                            tracing::warn!(
                                error = %error,
                                "Identity unavailable, clearing transcode fallback candidates"
                            );
                            media_items.clear();
                        }
                    }
                }
            }

            media_items.sort_by(|x, y| {
                let current_density = x.height.unwrap() * x.width.unwrap();
                let next_density = y.height.unwrap() * y.width.unwrap();

                if current_density < next_density {
                    return std::cmp::Ordering::Greater;
                } else {
                    return std::cmp::Ordering::Less;
                }
            });
            // dbg!(&media_items.iter().map(|x| x.video_resolution.clone()));
            // for now just select a random fallback
            for (index, media) in media_items.iter().enumerate() {
                if available_media_ids.contains(&media.id) {
                    if queries.get("maxVideoBitrate").is_some()
                        || queries.get("videoBitrate").is_some()
                    {
                        // tracing::trace!(
                        //     "Video has max bitrate which always forces transcode. Forcing max quality for fallback {}",
                        //     media,
                        // );

                        // if same resolution we can assume it will transcode again. Fallback to another resolution
                        let resolution = media
                            .video_resolution
                            .clone()
                            .unwrap()
                            .to_lowercase();
                        if resolution == fallback_for {
                            continue;
                        }

                        // check if requested falls into a resolution range. Either we remove the max bitrate or allow it
                        //let requested_bitrate: i64 = queries
                        //    .get("videoBitrate")
                        //    .unwrap_or_else(|| queries.get("maxVideoBitrate").unwrap()).parse().unwrap();

                        //if (resolution == "1080" && requested_bitrate >= 8000)
                        //    || (resolution == "720"
                        //        && requested_bitrate >= 2000)
                        //{
                        //    force_maximum_quality
                        //        .handle(req, depot, res, ctrl)
                        //        .await;
                        //    queries = req.queries().clone();
                        //}
                    }

                    // force_maximum_quality
                    tracing::debug!(
                        "Video transcode fallback from {} to {}",
                        selected_media,
                        media,
                    );
                    // let mut media_queries = req.queries().clone();
                    queries.remove("mediaIndex");
                    queries.insert("mediaIndex".to_string(), index.to_string());
                    queries.remove("directStream");
                    queries.insert("directStream".to_string(), "1".to_string());

                    if requested_bitrate.is_none() {
                        queries.remove("directPlay");
                        queries
                            .insert("directPlay".to_string(), "1".to_string());
                    }

                    queries.remove("subtitles");
                    queries.insert("subtitles".to_string(), "auto".to_string());

                    replace_query(queries.clone(), req);
                    // processed_media_indexes.append(selected_media.id);
                    // available_media_ids.remove(selected_media.id);

                    if media.video_resolution.clone().unwrap().to_lowercase()
                        != fallback_for
                    {
                        fallback_selected = true;
                        break;
                    }

                    let status: TranscodingStatus =
                        get_transcoding_for_request(req).await?;
                    available_media_ids.retain(|x| *x != media.id);
                    if status.is_transcoding && available_media_ids.len() != 0 {
                        tracing::debug!(
                            "Fallback is transcoding, getting another fallback",
                        );
                        continue;
                    }
                    // let mut transcode: MediaContainerWrapper<MediaContainer> =
                    //     from_reqwest_response(response).await?;
                    fallback_selected = true;
                    break;
                }
            }
            if !fallback_selected {
                tracing::debug!("No suitable fallback found");
                replace_query(original_queries, req);
            }
        }
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
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) -> Result<(), anyhow::Error> {
    let config: Config = Config::dynamic(req).extract().unwrap();
    if !config.resolution_policy_enabled {
        return Ok(());
    }

    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return Err(e);
        }
    };

    let identity = match plex_client.get_current_user().await {
        Ok(identity) => identity,
        Err(error) => {
            if config.resolution_policy_fail_closed {
                tracing::warn!(
                    error = %error,
                    "Identity unavailable, failing closed on playback request"
                );
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                ctrl.skip_rest();
                return Ok(());
            }
            tracing::warn!(
                error = %error,
                "Identity unavailable, failing open on playback request"
            );
            return Ok(());
        }
    };

    let policy = resolve_policy(
        &config.user_resolution_policies,
        config.resolution_default,
        config.hidden_collections.as_deref().unwrap_or(&[]),
        &identity,
    );
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
            if config.resolution_policy_fail_closed {
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                ctrl.skip_rest();
                return Ok(());
            }
            return Ok(());
        }
    };

    let media = &item.media_container.metadata[0].media;
    if media.is_empty() {
        return Ok(());
    }

    // Record which parts belong to permitted versions so direct
    // /library/parts requests can be validated later. Scoped to this
    // account and its current policy; never shared across users.
    cache_part_policy(media, &policy, &identity.uuid).await;

    let screen_resolution = context
        .screen_resolution
        .get(0)
        .map(|r| (r.width as i64, r.height as i64));

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
            let best_index =
                media.iter().position(|m| m.id == best.id).unwrap();
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
async fn auto_select_version(req: &mut Request) {
    let context: PlexContext = req.extract().await.unwrap();
    let plex_client = match PlexClient::from_context(&context) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build Plex client from request context");
            return;
        }
    };
    let mut queries = req.queries().clone();
    let media_index = queries.get("mediaIndex");

    if media_index.is_some() && media_index.unwrap() != "-1" {
        tracing::debug!(
            "Skipping auto selected as client specified a media index"
        );
        return;
    }

    if context.screen_resolution.len() == 0 {
        tracing::debug!(
            "Skipping auto selected as no screen resolution has been specified"
        );
        return;
    }

    if queries.get("path").is_some() {
        let item = plex_client
            .get_item_by_key(req.queries().get("path").unwrap().to_string())
            .await
            .unwrap();

        if item.media_container.metadata[0].media.len() <= 1 {
            tracing::debug!(
                "Only one media version available, skipping auto select"
            );
            return;
        }

        let mut requested_bitrate: Option<i64> = None;
        if queries.get("videoBitrate").is_some() {
            requested_bitrate =
                Some(queries.get("videoBitrate").unwrap().parse().unwrap());
        } else if queries.get("maxVideoBitrate").is_some() {
            requested_bitrate =
                Some(queries.get("maxVideoBitrate").unwrap().parse().unwrap());
        }

        let mut media = item.media_container.metadata[0].media.clone();
        let device_density = context.screen_resolution[0].height
            * context.screen_resolution[0].width;
        if media.len() > 1 {
            media.sort_by(|x, y| {
                let current_density = x.height.unwrap() * x.width.unwrap();
                let next_density = y.height.unwrap() * y.width.unwrap();
                let q = current_density - device_density;
                let qq = next_density - device_density;

                if q > qq {
                    return std::cmp::Ordering::Greater;
                } else {
                    return std::cmp::Ordering::Less;
                }
            })
        }

        for (index, m) in
            item.media_container.metadata[0].media.iter().enumerate()
        {
            if m.id == media[0].id {
                tracing::debug!("Auto selected {}", m);
                queries.remove("mediaIndex");
                queries.insert("mediaIndex".to_string(), index.to_string());
                // directPlay is meant for the first media item
                if requested_bitrate.is_none() {
                    queries.remove("directPlay");
                    queries.insert("directPlay".to_string(), "1".to_string());
                }

                queries.remove("subtitles");
                queries.insert("subtitles".to_string(), "auto".to_string());
            }
        }
    }
    replace_query(queries, req);
}

#[handler]
async fn ping(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    res.render("pong!")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use rstest::rstest;
    use salvo::prelude::*;
    use salvo::test::{ResponseExt, TestClient};

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

        // Config::dynamic() unwraps the HOST header, so the test client must
        // send one like a real request would. `path` already starts with a
        // slash, so no separator is added here.
        let content =
            TestClient::get(format!("http://127.0.0.1:5800{}", &path))
                .add_header("HOST", &mock_server.address().to_string(), true)
                .add_header("X-Plex-Token", "fakeID", true)
                .add_header("X-Plex-Client-Identifier", "fakeID", true)
                .add_header("Accept", "application/json", true)
                .send((&service))
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
        let _res = TestClient::post(format!(
            "http://127.0.0.1:5800/:/timeline?state=stopped&ratingKey=1&key=/library/metadata/1"
        ))
        .add_header("HOST", &mock_server.address().to_string(), true)
        .add_header("X-Plex-Token", "fakeID", true)
        .add_header("X-Plex-Client-Identifier", "fakeID", true)
        .send((&service))
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
            let mut res =
                TestClient::get("http://127.0.0.1:5800/web/test-asset.js")
                    .add_header("HOST", &server.address().to_string(), true)
                    .send((&service))
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
                .send((&service))
                .await
                .take_string()
                .await
                .unwrap();
        assert_eq!(content, "pong!");
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

        let service = Service::new(super::route());
        // Realistic client URL: the field-shaping/client-metadata noise a
        // real Plex Web session attaches must not prevent disk-cache reuse.
        let url = "http://127.0.0.1:5800/library/sections/6/all?excludeFields=summary&includeGeolocation=1&X-Plex-Client-Identifier=testclient&X-Plex-Container-Size=50&X-Plex-Container-Start=0";

        // Monday: unrestricted account, 4K version is visible.
        let content_monday = TestClient::get(url)
            .add_header("HOST", &server.address().to_string(), true)
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

        let content_tuesday = TestClient::get(url)
            .add_header("HOST", &server.address().to_string(), true)
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
