use crate::config::Config;
use crate::models::{MediaContainer, MediaContainerWrapper};
use crate::plex_client::PartMediaClassification;
use crate::resolution_policy::ResolvedIdentity;
use moka::future::Cache;
use salvo::{async_trait, Depot, FlowCtrl, Handler, Request, Response};
use std::sync::Arc;
use std::time::Duration;

pub const APP_STATE_KEY: &str = "replex.app_state";

/// Process-level dependencies built once and shared by every request.
/// Reqwest clients are cheap to clone and retain their underlying connection
/// pools, so keeping them here avoids rebuilding TLS and pooling state in hot
/// request paths.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub plex_http: reqwest_middleware::ClientWithMiddleware,
    pub proxy_http: reqwest::Client,
    pub identity_http: reqwest::Client,
    pub asset_http: reqwest::Client,
    pub metadata_cache: Cache<String, MediaContainerWrapper<MediaContainer>>,
    pub identity_cache: Cache<String, ResolvedIdentity>,
    pub part_media_cache: Cache<i64, PartMediaClassification>,
    pub server_machine_ids: Cache<String, String>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        config.validate()?;

        let plex_raw = reqwest::Client::builder()
            .gzip(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;
        let plex_http =
            reqwest_middleware::ClientBuilder::new(plex_raw).build();

        let proxy_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60 * 200))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;

        let identity_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;

        let asset_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;

        let metadata_cache = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(config.cache_ttl))
            .build();
        let identity_cache = Cache::builder()
            .max_capacity(1_000)
            .time_to_live(Duration::from_secs(config.identity_cache_ttl))
            .build();
        let part_media_cache = Cache::builder()
            .max_capacity(100_000)
            .time_to_live(Duration::from_secs(config.identity_cache_ttl))
            .build();
        let server_machine_ids = Cache::builder().max_capacity(10).build();

        crate::cache::configure_global_cache_ttl(config.cache_ttl);

        Ok(Self {
            config: Arc::new(config),
            plex_http,
            proxy_http,
            identity_http,
            asset_http,
            metadata_cache,
            identity_cache,
            part_media_cache,
            server_machine_ids,
        })
    }
}

#[derive(Clone)]
pub struct AppStateMiddleware {
    state: Arc<AppState>,
}

impl AppStateMiddleware {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Handler for AppStateMiddleware {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let _ = depot.insert(APP_STATE_KEY, self.state.clone());
        ctrl.call_next(req, depot, res).await;
    }
}

pub fn from_depot(depot: &Depot) -> anyhow::Result<Arc<AppState>> {
    depot
        .get::<Arc<AppState>>(APP_STATE_KEY)
        .cloned()
        .map_err(|_| {
            anyhow::anyhow!("shared application state missing from request")
        })
}
