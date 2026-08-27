use std::time::Duration;

use crate::config::Config;
use crate::models::*;
use crate::resolution_policy::UserIdentity;
use crate::utils::*;
use anyhow::Result;
use std::collections::HashMap;

use crate::cache::GLOBAL_CACHE;
use async_recursion::async_recursion;
use futures_util::Future;
use futures_util::TryStreamExt;
use http::header::ACCEPT_LANGUAGE;
use http::header::CONNECTION;
use http::header::COOKIE;
use http::header::FORWARDED;
use http::HeaderMap;
use http::Uri;
// use hyper::client::HttpConnector;
// use hyper::Body;
use moka::future::Cache;
//use moka::future::ConcurrentCacheExt;
use once_cell::sync::Lazy;
use reqwest::header;
use reqwest::header::ACCEPT;
use reqwest_retry::{default_on_request_failure, Retryable, RetryableStrategy};
use salvo::Error;
use salvo::Request;
// use hyper::client::HttpConnector;

static CACHE: Lazy<Cache<String, MediaContainerWrapper<MediaContainer>>> =
    Lazy::new(|| {
        let c: Config = Config::figment().extract().unwrap();
        Cache::builder()
            .max_capacity(10000)
            .time_to_live(Duration::from_secs(c.cache_ttl))
            .eviction_listener(|key, value, cause| {
                //println!("Evicted ({key:?},{value:?}) because {cause:?}")
            })
            .build()
    });

/// Verified Plex account identities keyed by token hash. Tokens themselves
/// are never stored or logged.
static IDENTITY_CACHE: Lazy<Cache<String, UserIdentity>> = Lazy::new(|| {
    let c: Config = Config::figment().extract().unwrap();
    Cache::builder()
        .max_capacity(1000)
        .time_to_live(Duration::from_secs(c.identity_cache_ttl))
        .build()
});

/// Media part permission decisions, keyed by
/// `(verified user uuid, policy fingerprint, part id)`.
///
/// The decision "is this part permitted" is a pure function of the
/// requesting account's policy and the part's media version, so the key
/// MUST capture both. Keying by part id alone would let one account's
/// permission decision (e.g. a 4K-permitted account priming a 4K part)
/// authorise a different account that is restricted to 1080p.
pub type PartPolicyKey = (String, String, i64);

pub static PART_POLICY_CACHE: Lazy<Cache<PartPolicyKey, bool>> = Lazy::new(|| {
    let c: Config = Config::figment().extract().unwrap();
    Cache::builder()
        .max_capacity(100_000)
        .time_to_live(Duration::from_secs(c.identity_cache_ttl))
        .build()
});

/// Stable fingerprint of every policy input that `media_allowed` consults.
/// Keep this in sync with `media_allowed`: if it ever reads more policy
/// fields, they must be folded in here.
pub fn part_policy_fingerprint(policy: &crate::resolution_policy::ResolutionPolicy) -> String {
    format!("{:?}", policy.limit)
}

/// Build the cache key for one part-permission decision.
pub fn part_policy_key(
    user_uuid: &str,
    policy: &crate::resolution_policy::ResolutionPolicy,
    part_id: i64,
) -> PartPolicyKey {
    (
        user_uuid.to_string(),
        part_policy_fingerprint(policy),
        part_id,
    )
}

/// Record the permitted/blocked status of every part of every media version,
/// scoped to the verified account and its current policy.
pub async fn cache_part_policy(
    media: &[Media],
    policy: &crate::resolution_policy::ResolutionPolicy,
    user_uuid: &str,
) {
    for m in media {
        let allowed = crate::resolution_policy::media_allowed(m, policy);
        for part in &m.parts {
            PART_POLICY_CACHE
                .insert(part_policy_key(user_uuid, policy, part.id), allowed)
                .await;
        }
    }
}

/// Errors raised while resolving the authenticated Plex account.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The request token was rejected by plex.tv (401/403).
    #[error("invalid or expired plex token")]
    InvalidToken,
    /// No token was present on the request.
    #[error("no plex token on request")]
    MissingToken,
    /// plex.tv could not be reached or returned an unexpected response.
    #[error("identity lookup failed: {0}")]
    Upstream(#[from] anyhow::Error),
}

/// Hash a token for cache keying so raw tokens never sit in cache keys.
fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    data_encoding::HEXLOWER.encode(&digest)
}

/// Cached machineIdentifier of the upstream Plex server, keyed by upstream
/// host, used to recognise which resource entry in a shared user's
/// plex.tv resources list is ours.
static SERVER_MACHINE_IDS: Lazy<Cache<String, String>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(10)
        .build()
});

struct Retry401;
impl RetryableStrategy for Retry401 {
    fn handle(
        &self,
        res: &std::result::Result<reqwest::Response, reqwest_middleware::Error>,
    ) -> Option<Retryable> {
        match res {
            Ok(success) if success.status() == 401 => {
                Some(Retryable::Transient)
            }
            Ok(success) => None,
            // otherwise do not retry a successful request
            Err(error) => default_on_request_failure(error),
        }
    }
}

/// TODO: Implement clone
#[derive(Debug, Clone)]
pub struct PlexClient {
    pub http_client: reqwest_middleware::ClientWithMiddleware,
    pub context: PlexContext,
    pub host: String, // TODO: Dont think this supposed to be here. Should be higher up
    pub cache: Cache<String, MediaContainerWrapper<MediaContainer>>,
    pub default_headers: header::HeaderMap,
}

impl PlexClient {
    // TODO: Handle 404s/500 etc
    // TODO: Map reqwest response and error to salvo
    pub async fn get(&self, path: String) -> Result<reqwest::Response, Error> {
        let mut req = Request::default();
        *req.method_mut() = http::Method::GET;
        req.set_uri(Uri::builder().path_and_query(path).build().unwrap());
        self.request(&mut req).await
    }

    pub async fn request(
        &self,
        req: &Request,
    ) -> Result<reqwest::Response, Error> {
        let url = format!(
            "{}{}",
            self.host,
            &req.uri().clone().path_and_query().unwrap()
        );
        let mut headers = self.default_headers.clone();
        for (key, value) in req.headers().iter() {
            if key != ACCEPT && key != http::header::HOST {
                headers.insert(key, value.clone());
            }
        }
        //let mut headers = req.headers_mut().clone();
        //headers.remove(ACCEPT); // remove accept as we always do json request
        //dbg!(&headers);
        //dbg!(&url);
        let res = self
            .http_client
            .request(req.method().clone(), url)
            .headers(headers)
            .send()
            .await
            .map_err(Error::other)?;

        Ok(res)
    }

    pub async fn proxy_request(
        &self,
        req: &Request,
    ) -> Result<reqwest::Response, Error> {
        let url = format!(
            "{}{}?{}",
            self.host,
            encode_url_path(&url_path_getter(req).unwrap()),
            url_query_getter(req).unwrap()
        );
        //dbg!(&req);
        //dbg!(&url);
        //dbg!(&req.uri().clone().query().unwrap().to_string());
        let mut headers = req.headers().clone();
        headers.remove(ACCEPT); // remove accept as we always do json request
        headers.remove(http::header::HOST);
        let res = self
            .http_client
            .request(req.method().clone(), url)
            //.execute(req)
            .headers(headers)
            .send()
            .await
            .map_err(Error::other)?;
        //dbg!(&res);
        Ok(res)
    }

    pub async fn get_section_collections(
        &self,
        id: i64,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let res = self
            .get(format!("/library/sections/{}/collections", id))
            .await
            .unwrap();

        let container: MediaContainerWrapper<MediaContainer> =
            from_reqwest_response(res)
                .await
                .expect("Cannot get MediaContainerWrapper from response");

        Ok(container)
    }

    pub async fn get_collection_children(
        &self,
        id: i64,
        offset: Option<i32>,
        limit: Option<i32>,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let mut path = format!("/library/collections/{}/children", id);

        if offset.is_some() {
            path =
                format!("{}?X-Plex-Container-Start={}", path, offset.unwrap());
        }

        if limit.is_some() {
            path = format!("{}&X-Plex-Container-Size={}", path, limit.unwrap());
        }

        // we want guids for banners
        path = format!("{}&includeGuids=1", path);
        // dbg!(&path);

        let res = self.get(path).await.unwrap();
        if !res.status().is_success() {
            return Err(anyhow::anyhow!(format!(
                "unexpected status code: status = {}",
                res.status()
            )));
        }

        let container: MediaContainerWrapper<MediaContainer> =
            from_reqwest_response(res).await.unwrap();
        Ok(container)
    }

    #[async_recursion]
    pub async fn load_collection_children_recursive(
        &self,
        id: i64,
        offset: i32,
        limit: i32,
        original_limit: i32,
    ) -> anyhow::Result<MediaContainerWrapper<MediaContainer>> {
        let config: Config = Config::figment().extract().unwrap();
        let mut c = self
            .get_collection_children(id, Some(offset), Some(limit))
            .await?;
        c.media_container.children_mut().retain(|x| !x.is_watched());
        c.media_container
            .children_mut()
            .truncate(original_limit as usize);

        Ok(c)
    }

    pub async fn get_collection(
        &self,
        id: i32,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let res = self.get(format!("/library/collections/{}", id)).await?;

        if res.status() == 404 {
            return Err(salvo::http::StatusError::not_found().into());
        }

        let container: MediaContainerWrapper<MediaContainer> =
            from_reqwest_response(res).await.unwrap();
        Ok(container)
    }

    // theres actually a global endpoint https://plex.sjoerdarendsen.dev/library/all?show.collection=2042780&collection=2042780&X-Plex-Container-Start=0&X-Plex-Container-Size=72
    pub async fn get_collection_total_size_unwatched(
        &self,
        section_id: i32,
        collection_index: i32,
        r#type: String,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let mut path = format!("/library/sections/{}/all?X-Plex-Container-Start=0&X-Plex-Container-Size=0", section_id);
        // dbg!(&path);

        if r#type == "show" {
            path = format!(
                "{}&show.unwatchedLeaves=1&show.collection={}",
                path, collection_index
            );
        }

        if r#type == "movie" {
            path = format!(
                "{}&movie.unwatched=1&movie.collection={}",
                path, collection_index
            );
        }
        // dbg!(&path);
        let res = self.get(path).await?;

        if res.status() == 404 {
            return Err(salvo::http::StatusError::not_found().into());
        }

        let container: MediaContainerWrapper<MediaContainer> =
            from_reqwest_response(res).await.unwrap();
        Ok(container)
    }

    pub async fn get_hubs(
        &self,
        id: i32,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let res = self.get("/hubs".to_string()).await.unwrap();
        let container: MediaContainerWrapper<MediaContainer> =
            from_reqwest_response(res).await.unwrap();
        Ok(container)
    }

    pub async fn get_item_by_key(
        self,
        key: String,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let res = self.get(key).await.unwrap();
        let container: MediaContainerWrapper<MediaContainer> =
            from_reqwest_response(res).await.unwrap();
        Ok(container)
    }

    /// Resolve the authenticated Plex account for this client's request token.
    ///
    /// The token is verified against plex.tv; the resulting identity is the
    /// only trusted basis for per-user policies. Results are cached by token
    /// hash for `identity_cache_ttl` seconds. Authentication failures evict
    /// any cached identity immediately.
    pub async fn get_current_user(
        &self,
    ) -> Result<UserIdentity, IdentityError> {
        let token = self
            .context
            .token
            .clone()
            .ok_or(IdentityError::MissingToken)?;

        let cache_key = hash_token(&token);

        if let Some(identity) = IDENTITY_CACHE.get(&cache_key).await {
            tracing::debug!(
                username = %identity.username,
                uuid = %identity.uuid,
                "Resolution identity resolved (cached)"
            );
            return Ok(identity);
        }

        let identity = self.fetch_current_user(&token).await?;

        tracing::info!(
            username = %identity.username,
            uuid = %identity.uuid,
            "Resolution identity resolved"
        );

        IDENTITY_CACHE.insert(cache_key, identity.clone()).await;

        Ok(identity)
    }

    async fn fetch_current_user(
        &self,
        token: &str,
    ) -> Result<UserIdentity, IdentityError> {
        let config: Config = Config::figment().extract().unwrap();
        let base = config.identity_api_base();
        let url = format!("{}/api/v2/user", base);

        let mut headers = header::HeaderMap::new();
        headers.insert(
            "X-Plex-Token",
            header::HeaderValue::from_str(token).map_err(|e| {
                IdentityError::Upstream(anyhow::anyhow!(
                    "bad token header: {e}"
                ))
            })?,
        );
        headers.insert(
            "X-Plex-Client-Identifier",
            self.client_identifier_header(),
        );
        headers.insert(
            ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );

        let res = self
            .http_client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| {
                IdentityError::Upstream(anyhow::anyhow!(
                    "plex.tv request failed: {e}"
                ))
            })?;

        match res.status() {
            salvo::http::StatusCode::OK => {}
            salvo::http::StatusCode::UNAUTHORIZED
            | salvo::http::StatusCode::FORBIDDEN => {
                let body = res.text().await.unwrap_or_default();
                tracing::warn!(
                    body_snippet = %&body.chars().take(300).collect::<String>(),
                    "plex.tv rejected global identity verification, trying shared-token resolution"
                );
                // Server-scoped tokens (issued to shared users when a client
                // connects to an invited server) are rejected by
                // plex.tv/api/v2/user. Resolve them via the resources
                // endpoint instead, which reveals which shared user the
                // token belongs to.
                if let Some(identity) = self.resolve_shared_user(token).await? {
                    return Ok(identity);
                }
                // plex.tv rejects some device-scoped shared tokens outright;
                // match the token against the admin's shared-server list.
                if let Some(identity) = self.resolve_via_shared_servers(token).await? {
                    return Ok(identity);
                }
                // Last resort for clients whose tokens are opaque to
                // plex.tv (e.g. preview apps using PMS-scoped session
                // tokens): trust the X-Plex-Username header. Opt-in only,
                // because it is spoofable by anyone with server access.
                let config: Config = Config::figment().extract().unwrap();
                if config.allow_username_fallback {
                    if let Some(username) = self
                        .context
                        .username
                        .clone()
                        .filter(|u| !u.is_empty())
                    {
                        tracing::warn!(
                            username = %username,
                            "Identity resolved via unverified username header (allow_username_fallback)"
                        );
                        return Ok(UserIdentity {
                            id: 0,
                            uuid: format!("unverified-{username}"),
                            username,
                        });
                    }
                }
                // Deterministic per-device binding: some TV clients use
                // PMS-scoped session tokens invisible to every plex.tv
                // endpoint. An admin-configured clientIdentifier -> username
                // map is the only reliable identity source for them.
                let config: Config = Config::figment().extract().unwrap();
                if let Some(cid) = &self.context.client_identifier {
                    if let Some(username) = config.client_identity_map.get(cid) {
                        tracing::warn!(
                            username = %username,
                            client_id = %cid,
                            "Identity resolved via client identity map"
                        );
                        return Ok(UserIdentity {
                            id: 0,
                            uuid: format!("cid-{cid}"),
                            username: username.clone(),
                        });
                    }
                }
                return Err(IdentityError::InvalidToken);
            }
            status => {
                return Err(IdentityError::Upstream(anyhow::anyhow!(
                    "unexpected status from identity API: {status}"
                )))
            }
        }

        #[derive(Debug, serde::Deserialize)]
        struct RawUser {
            id: i64,
            uuid: String,
            #[serde(default)]
            username: Option<String>,
            #[serde(default)]
            title: Option<String>,
        }

        let raw: RawUser = res.json().await.map_err(|e| {
            IdentityError::Upstream(anyhow::anyhow!(
                "identity response parse failed: {e}"
            ))
        })?;

        let username = raw
            .username
            .or(raw.title)
            .unwrap_or_else(|| format!("user-{}", raw.uuid));

        Ok(UserIdentity {
            id: raw.id,
            uuid: raw.uuid,
            username,
        })
    }

    /// Resolve a *server-scoped* token (issued to shared users) into a
    /// `UserIdentity`.
    ///
    /// plex.tv/api/v2/user rejects server-scoped tokens, but the resources
    /// endpoint accepts them and marks our server's entry with `sourceTitle`
    /// — the username of the account the token belongs to.
    async fn resolve_shared_user(
        &self,
        token: &str,
    ) -> Result<Option<UserIdentity>, IdentityError> {
        let machine_id = self.server_machine_id().await?;

        let base = Config::figment()
            .extract::<Config>()
            .map(|c| c.identity_api_base())
            .unwrap_or_else(|_| "https://plex.tv".to_string());
        let url = format!("{base}/api/v2/resources?includeHttps=1&includeRelay=0");
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "X-Plex-Token",
            header::HeaderValue::from_str(token)
                .map_err(|e| IdentityError::Upstream(anyhow::anyhow!("bad token header: {e}")))?,
        );
        headers.insert(
            "X-Plex-Client-Identifier",
            self.client_identifier_header(),
        );
        headers.insert(ACCEPT, header::HeaderValue::from_static("application/json"));

        tracing::debug!(url = %url, "Shared-token resources lookup");
        let res = self
            .http_client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| IdentityError::Upstream(anyhow::anyhow!("resources request failed: {e}")))?;
        tracing::debug!(status = %res.status(), "Shared-token resources response");

        match res.status() {
            salvo::http::StatusCode::OK => {}
            salvo::http::StatusCode::UNAUTHORIZED
            | salvo::http::StatusCode::FORBIDDEN
            | salvo::http::StatusCode::NOT_FOUND => {
                return Ok(None); // genuinely invalid token or no visibility
            }
            status => {
                return Err(IdentityError::Upstream(anyhow::anyhow!(
                    "unexpected status from resources API: {status}"
                )))
            }
        }

        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawResource {
            #[serde(default)]
            client_identifier: Option<String>,
            #[serde(default)]
            provides: Option<String>,
            #[serde(default)]
            source_title: Option<String>,
            #[serde(default)]
            owner_id: Option<i64>,
            #[serde(default)]
            access_token: Option<String>,
        }

        let resources: Vec<RawResource> = res.json().await.map_err(|e| {
            IdentityError::Upstream(anyhow::anyhow!("resources response parse failed: {e}"))
        })?;

        tracing::debug!(
            machine_id = %machine_id,
            count = resources.len(),
            entries = ?resources.iter().map(|r| (r.client_identifier.clone(), r.provides.clone(), r.source_title.clone())).collect::<Vec<_>>(),
            "Shared-token resources scan"
        );
        for r in resources {
            let is_ours = r.client_identifier.as_deref() == Some(machine_id.as_str())
                && r.provides.as_deref().map(|p| p.contains("server")).unwrap_or(false);
            if !is_ours {
                continue;
            }
            match r.source_title.filter(|s| !s.is_empty()) {
                Some(username) => {
                    tracing::info!(
                        username = %username,
                        "Shared-token identity resolved via plex.tv resources"
                    );
                    return Ok(Some(UserIdentity {
                        id: r.owner_id.unwrap_or(0),
                        // Shared users have no distinct uuid exposed here; use
                        // a stable synthetic form. Policies should target
                        // shared users by USERNAME.
                        uuid: format!("shared-{username}"),
                        username,
                    }));
                }
                None => {
                    // Our entry without sourceTitle means an owner-scoped
                    // token; shouldn't reach here (global path succeeded),
                    // but treat as unresolved rather than guessing.
                    return Ok(None);
                }
            }
        }

        // Token is valid but has no access to THIS server: not one of ours.
        Ok(None)
    }

    /// Resolve a shared user by matching the request token against the
    /// admin's shared-server access token list on plex.tv.
    ///
    /// Some shared-user tokens are device/session-scoped and rejected by
    /// every plex.tv /api/v2 endpoint, even though the media server itself
    /// accepts them. The admin view (`/api/servers/{id}/shared_servers`,
    /// authed with REPLEX_TOKEN) lists every outstanding shared accessToken
    /// with its username, giving a deterministic identity source that does
    /// not depend on plex.tv accepting the requestor's token.
    async fn resolve_via_shared_servers(
        &self,
        token: &str,
    ) -> Result<Option<UserIdentity>, IdentityError> {
        let config: Config = Config::figment().extract().unwrap();
        let Some(admin_token) = config.token.clone() else {
            tracing::debug!("shared-servers lookup skipped: no admin token configured");
            return Ok(None);
        };
        let machine_id = self.server_machine_id().await?;

        let base = config.identity_api_base();
        let url = format!("{base}/api/servers/{machine_id}/shared_servers");
        let res = self
            .http_client
            .get(&url)
            .header("X-Plex-Token", admin_token)
            .header("X-Plex-Client-Identifier", self.client_identifier_header())
            .header(ACCEPT, "application/xml")
            .send()
            .await
            .map_err(|e| {
                IdentityError::Upstream(anyhow::anyhow!(
                    "shared_servers request failed: {e}"
                ))
            })?;

        if res.status() != salvo::http::StatusCode::OK {
            tracing::warn!(
                status = %res.status(),
                "shared_servers lookup failed"
            );
            return Ok(None);
        }

        // The response is XML: <SharedServer username="..." accessToken="..." .../>
        // Scan each entry and match on the access token.
        let body = res.text().await.unwrap_or_default();
        for tag in body.split("<SharedServer ").skip(1) {
            let attrs = Self::parse_xml_attrs(tag.split('>').next().unwrap_or(""));
            let entry_token = attrs.get("accessToken").map(|s| s.as_str());
            if entry_token != Some(token) {
                continue;
            }
            let Some(username) = attrs.get("username").filter(|u| !u.is_empty()) else {
                continue;
            };
            tracing::info!(
                username = %username,
                "Shared-token identity resolved via admin shared_servers"
            );
            return Ok(Some(UserIdentity {
                id: attrs
                    .get("userID")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0),
                uuid: format!("shared-{username}"),
                username: username.clone(),
            }));
        }
        Ok(None)
    }

    /// Extract key="value" pairs from an XML attribute string.
    fn parse_xml_attrs(s: &str) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        let mut rest = s.trim();
        while let Some(eq) = rest.find('=') {
            let key = rest[..eq].trim().to_string();
            let after = rest[eq + 1..].trim_start();
            if !after.starts_with('"') {
                break;
            }
            match after[1..].find('"') {
                Some(end) => {
                    map.insert(key, after[1..1 + end].to_string());
                    rest = after[end + 2..].trim_start();
                }
                None => break,
            }
        }
        map
    }

    /// plex.tv v2 endpoints reject requests without X-Plex-Client-Identifier.
    /// Use the client's when present, otherwise a stable proxy identifier.
    fn client_identifier_header(&self) -> header::HeaderValue {
        let value = self
            .context
            .client_identifier
            .clone()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "replex-resolution-proxy".to_string());
        header::HeaderValue::from_str(&value)
            .unwrap_or(header::HeaderValue::from_static("replex-resolution-proxy"))
    }

    async fn server_machine_id(&self) -> Result<String, IdentityError> {
        if let Some(id) = SERVER_MACHINE_IDS.get(&self.host).await {
            return Ok(id);
        }

        let url = format!("{}/", self.host);
        let mut headers = self.default_headers.clone();
        headers.insert(
            ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        let res = self
            .http_client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| IdentityError::Upstream(anyhow::anyhow!("server root request failed: {e}")))?;

        #[derive(Debug, serde::Deserialize)]
        struct RawRoot {
            #[serde(rename = "MediaContainer")]
            container: RawRootContainer,
        }
        #[derive(Debug, serde::Deserialize)]
        struct RawRootContainer {
            #[serde(rename = "machineIdentifier")]
            machine_identifier: String,
        }

        let body = res.text().await.unwrap_or_default();
        let root: RawRoot = serde_json::from_str(&body).map_err(|e| {
            tracing::warn!(
                snippet = %&body.chars().take(250).collect::<String>(),
                error = %e,
                "server root parse failed"
            );
            IdentityError::Upstream(anyhow::anyhow!("server root parse failed: {e}"))
        })?;

        let id = root.container.machine_identifier;
        SERVER_MACHINE_IDS.insert(self.host.clone(), id.clone()).await;
        Ok(id)
    }

    pub async fn get_cached(
        self,
        f: impl Future<Output = Result<MediaContainerWrapper<MediaContainer>>>,
        name: String,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let cache_key = self.generate_cache_key(name.clone());
        let cached = self.get_cache(&cache_key).await?;

        if cached.is_some() {
            return Ok(cached.unwrap());
        }
        let r = f.await?;
        self.insert_cache(cache_key, r.clone()).await;
        Ok(r)
    }

    pub async fn get_hero_art(self, uuid: String) -> Option<String> {
        //tracing::debug!(uuid = uuid, "Loading hero art from plex");
        let cache_key = format!("{}:hero_art", uuid);

        let cached_result: Option<Option<String>> =
            GLOBAL_CACHE.get(cache_key.as_str()).await;

        if cached_result.is_some() {
            //tracing::debug!("Returning cached version");
            return cached_result.unwrap();
        }

        let mut container: MediaContainerWrapper<MediaContainer> =
            match self.get_provider_data(&uuid).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        uuid = uuid,
                        error = %e,
                        "Problem loading provider metadata."
                    );
                    return None;
                    MediaContainerWrapper::default()
                }
            };

        let metadata = container.media_container.children_mut().get(0);
        let mut image: Option<String> = None;
        if metadata.is_some() {
            for i in &metadata.unwrap().images {
                if i.r#type == "coverArt" {
                    image = Some(i.url.clone());
                    break;
                }
            }
        }

        if image.is_none() {
            tracing::warn!(uuid = uuid, "No hero image found on plex");
        }

        image.as_ref()?; // dont return and dont cache, let us just retry next time.

        //tracing::debug!("Hero image found");

        let cache_expiry = crate::cache::Expiration::Month;
        let _ = GLOBAL_CACHE
            .insert(cache_key, image.clone(), cache_expiry)
            .await;

        image
    }

    pub async fn get_provider_data(
        self,
        uuid: &String,
    ) -> Result<MediaContainerWrapper<MediaContainer>> {
        let config: Config = Config::figment().extract().unwrap();
        let url = format!(
            "https://discover.provider.plex.tv/library/metadata/{}",
            uuid
        );

        let mut req = reqwest::Request::new(
            http::Method::GET,
            url.parse::<url::Url>().unwrap(),
        );
        let mut headers = HeaderMap::new();

        //endpoint is buggy, if llex has a cached version then it doesnt need a plex token
        // but if not cached then a server admin token is needed
        let mut token = config.token.clone();
        if token.is_some() {
            headers.insert(
                "X-Plex-Token",
                header::HeaderValue::from_str(token.unwrap().as_str()).unwrap(),
            );
        };

        headers.insert(
            "Accept",
            header::HeaderValue::from_static("application/json"),
        );
        *req.headers_mut() = headers;

        let res = self.http_client.execute(req).await.map_err(Error::other)?;

        if res.status() != salvo::http::StatusCode::OK {
            return Err(anyhow::anyhow!(format!(
                "unexpected status code: status = {}",
                res.status()
            )));
        }

        let container: MediaContainerWrapper<MediaContainer> =
            from_reqwest_response(res).await?;
        Ok(container)
    }

    async fn get_cache(
        &self,
        cache_key: &str,
    ) -> Result<Option<MediaContainerWrapper<MediaContainer>>> {
        Ok(self.cache.get(cache_key).await)
    }

    async fn insert_cache(
        &self,
        cache_key: String,
        container: MediaContainerWrapper<MediaContainer>,
    ) {
        self.cache.insert(cache_key, container).await;
    }

    fn generate_cache_key(&self, name: String) -> String {
        format!("{}:{}", name, self.context.token.clone().unwrap())
    }

    pub fn from_context(context: &PlexContext) -> Self {
        let config: Config = Config::figment().extract().unwrap();
        let token = context
            .clone()
            .token
            .expect("Expected to have an token in header or query");
        let client_identifier = context.clone().client_identifier;
        let platform = context.platform.clone().unwrap_or_default();

        //let req_headers = req.headers().clone();
        let mut headers = header::HeaderMap::new();
        let headers_map = HashMap::from([
            ("X-Plex-Token", context.token.clone()),
            ("X-Plex-Platform", Some(platform.clone().to_string())),
            (
                "X-Plex-Client-Identifier",
                context.client_identifier.clone(),
            ),
            ("X-Plex-Session-Id", context.session_id.clone()),
            (
                "X-Plex-Playback-Session-Id",
                context.playback_session_id.clone(),
            ),
            ("X-Plex-Product", context.product.clone()),
            ("X-Plex-Playback-Id", context.playback_id.clone()),
            ("X-Plex-Platform-Version", context.platform_version.clone()),
            ("X-Plex-Version", context.version.clone()),
            ("X-Plex-Features", context.features.clone()),
            ("X-Plex-Model", context.model.clone()),
            ("X-Plex-Device", context.device.clone()),
            ("X-Plex-Device-Name", context.device_name.clone()),
            ("X-Plex-Drm", context.drm.clone()),
            ("X-Plex-Text-Format", context.text_format.clone()),
            ("X-Plex-Http-Pipeline", context.http_pipeline.clone()),
            ("X-Plex-Provider-Version", context.provider_version.clone()),
            (
                "X-Plex-Device-Screen-Resolution",
                context.screen_resolution_original.clone(),
            ),
            (
                "X-Plex-Client-Capabilities",
                context.client_capabilities.clone(),
            ),
            ("X-Forwarded-For", context.forwarded_for.clone()),
            ("X-Real-Ip", context.real_ip.clone()),
            (&ACCEPT.as_str(), Some("application/json".to_string())),
            (&ACCEPT_LANGUAGE.as_str(), Some("en-US".to_string())),
            //(http::header::HOST.as_str(), Some(config.host.clone().unwrap())),
        ]);

        for (key, val) in headers_map {
            if val.is_some() {
                headers.insert(
                    key.clone(),
                    val.unwrap().as_str().parse().unwrap(),
                );
            }
        }

        //let target_uri: url::Url = url::Url::parse(config.host.clone().unwrap().as_str()).unwrap();
        //let target_host = target_uri.host().unwrap().to_string().clone();

        //headers.insert(
        //    http::header::HOST,
        //    header::HeaderValue::from_str(&target_host).unwrap(),
        //);

        Self {
            http_client: reqwest_middleware::ClientBuilder::new(
                reqwest::Client::builder()
                    //.default_headers(headers)
                    .gzip(true)
                    // Large libraries can exceed 30s on section queries;
                    // the outer request timeout is 200s so stay under it.
                    .timeout(Duration::from_secs(120))
                    .build()
                    .unwrap(),
            )
            .build(),
            default_headers: headers,
            host: config.host.unwrap(),
            context: context.clone(),
            //x_plex_token: token,
            //x_plex_client_identifier: client_identifier,
            //x_plex_platform: platform,
            cache: CACHE.clone(),
        }
    }

    // pub fn dummy() -> Self {
    //     let config: Config = Config::figment().extract().unwrap();
    //     let token = "DUMMY".to_string();
    //     let client_identifier: Option<String> = None;
    //     let platform: Platform = Platform::Generic;

    //     // Dont do the headers here. Do it in prepare function
    //     let mut headers = header::HeaderMap::new();
    //     headers.insert(
    //         "X-Plex-Token",
    //         header::HeaderValue::from_str(token.clone().as_str()).unwrap(),
    //     );
    //     headers.insert(
    //         "Accept",
    //         header::HeaderValue::from_static("application/json"),
    //     );
    //     headers.insert(
    //         "X-Plex-Platform",
    //         header::HeaderValue::from_str(platform.to_string().as_str())
    //             .unwrap(),
    //     );
    //     Self {
    //         http_client: reqwest::Client::builder()
    //             .default_headers(headers)
    //             .gzip(true)
    //             .timeout(Duration::from_secs(30))
    //             .build()
    //             .unwrap(),
    //         host: config.host.unwrap(),
    //         x_plex_token: token,
    //         x_plex_client_identifier: client_identifier,
    //         x_plex_platform: platform,
    //         cache: CACHE.clone(),
    //     }
    // }
}

// #[cfg(test)]
// mod tests {
//     use salvo::prelude::*;
//     use salvo::test::{ResponseExt, TestClient};
//     use crate::test_helpers::*;

//     #[tokio::test]
//     async fn test_hello_world() {
//         let service = Service::new(super::route());

//         let content = TestClient::get(format!("http://127.0.0.1:5800/{}", "hubs/promoted"))
//             .send((&service))
//             .await
//             .take_string()
//             .await
//             .unwrap();
//         assert_eq!(content, "Hello World");
//     }
// }

#[cfg(test)]
mod part_policy_cache_tests {
    use super::*;
    use crate::resolution_policy::{ResolutionLimit, ResolutionPolicy};

    fn policy(limit: ResolutionLimit) -> ResolutionPolicy {
        ResolutionPolicy {
            limit,
            max_bitrate: None,
            hidden_collections: vec![],
        }
    }

    /// Regression test for the cross-user part policy cache bug: the cache
    /// key must capture the verified user and the policy, so a 4K-permitted
    /// account priming a part can never authorise a 1080p-restricted account.
    #[test]
    fn keys_differ_across_users_and_policies() {
        let p1080 = policy(ResolutionLimit::P1080);
        let p4k = policy(ResolutionLimit::P2160);

        let user_a_1080 = part_policy_key("uuid-a", &p1080, 123);
        let user_b_1080 = part_policy_key("uuid-b", &p1080, 123);
        let user_a_4k = part_policy_key("uuid-a", &p4k, 123);

        assert_ne!(
            user_a_1080, user_b_1080,
            "same part, different accounts must not share decisions"
        );
        assert_ne!(
            user_a_1080, user_a_4k,
            "same account, changed policy must not reuse old decisions"
        );
        assert_eq!(
            user_a_1080,
            part_policy_key("uuid-a", &p1080, 123),
            "same user + policy must hit the same entry"
        );
    }

    #[test]
    fn fingerprint_tracks_the_limit() {
        assert_eq!(
            part_policy_fingerprint(&policy(ResolutionLimit::P1080)),
            part_policy_fingerprint(&policy(ResolutionLimit::P1080))
        );
        assert_ne!(
            part_policy_fingerprint(&policy(ResolutionLimit::P1080)),
            part_policy_fingerprint(&policy(ResolutionLimit::P2160))
        );
        assert_ne!(
            part_policy_fingerprint(&policy(ResolutionLimit::P1080)),
            part_policy_fingerprint(&policy(ResolutionLimit::Unlimited))
        );
    }
}
