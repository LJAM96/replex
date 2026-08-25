//! In-memory cache for Plex Web static assets (`/web/*`).
//!
//! PMS serves these files with `Cache-Control: no-cache` and no ETag or
//! Last-Modified validators, so browsers re-download every one of them on
//! each reload, each paying the full upstream round trip (~100ms observed).
//! The files are content-hashed immutable bundles; caching them here and
//! marking them immutable removes both costs after the first load.

use crate::cache::{Expiration, GLOBAL_CACHE};
use crate::config::Config;
use salvo::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use salvo::http::{HeaderValue, ResBody, StatusCode};
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const ASSET_CACHE_PREFIX: &str = "webasset:";

#[derive(Serialize, Deserialize, Clone)]
struct CachedAsset {
    content_type: Option<String>,
    body: Vec<u8>,
}

/// Cache policy per asset path. index.html and translation bundles change
/// with app updates so they stay short-lived; everything else under /web/
/// is content-hashed and safe to pin forever.
fn cache_policy_for(path: &str) -> &'static str {
    let volatile = path == "/web"
        || path == "/web/"
        || path.ends_with("/index.html")
        || path.contains("/translations/");
    if volatile {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    }
}


/// Fetch an asset from upstream once and stash the raw bytes.
async fn fetch_upstream(path: &str) -> Result<CachedAsset, StatusCode> {
    let config: Config = Config::figment().extract().unwrap();
    let host = config.host.unwrap_or_default();
    let url = format!("{}{}", host.trim_end_matches('/'), path);

    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if resp.status() != reqwest::StatusCode::OK {
        tracing::warn!(status = %resp.status(), path = %path, "upstream web asset fetch non-200");
        return Err(StatusCode::BAD_GATEWAY);
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    Ok(CachedAsset {
        content_type,
        body: body.to_vec(),
    })
}

/// Serve `/web/<**rest>` from the in-memory asset cache.
#[handler]
pub async fn serve_web_asset(req: &mut Request, res: &mut Response) {
    let path = req.uri().path().to_string();

    let asset = match GLOBAL_CACHE
        .get::<CachedAsset>(&format!("{}{}", ASSET_CACHE_PREFIX, path))
        .await
    {
        Some(asset) => asset,
        None => {
            let fetched = match fetch_upstream(&path).await {
                Ok(a) => a,
                Err(status) => {
                    res.status_code(status);
                    return;
                }
            };
            GLOBAL_CACHE
                .insert(
                    format!("{}{}", ASSET_CACHE_PREFIX, path),
                    fetched.clone(),
                    Expiration::Month,
                )
                .await
                .ok();
            fetched
        }
    };

    if let Some(ct) = asset.content_type {
        if let Ok(v) = HeaderValue::from_str(&ct) {
            res.headers_mut().insert(CONTENT_TYPE, v);
        }
    }
    res.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(cache_policy_for(&path)),
    );
    *res.body_mut() = ResBody::Once(bytes::Bytes::from(asset.body));
}
