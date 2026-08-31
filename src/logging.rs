//! Logging middleware
use std::time::Instant;

use crate::auth::{request_context, security_state, SecurityContextState};
use tracing::{Instrument, Level};

use salvo::http::{Request, Response, StatusCode};
use salvo::{async_trait, Depot, FlowCtrl, Handler};

/// A simple logger middleware.
#[derive(Default, Debug)]
pub struct Logger {}
impl Logger {
    /// Create new `Logger` middleware.
    #[inline]
    pub fn new() -> Self {
        Logger {}
    }
}

#[async_trait]
impl Handler for Logger {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let request = request_context(depot).ok();
        let request_id = request
            .as_ref()
            .map(|context| context.request_id.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let token_scope = request
            .as_ref()
            .map(|context| context.token_scope.clone())
            .unwrap_or_else(|| "anon".to_string());
        let requested_media_index = req
            .queries()
            .get("mediaIndex")
            .cloned()
            .unwrap_or_else(|| "-".to_string());
        let span = tracing::span!(
            Level::TRACE,
            "Request",
            request_id = %request_id,
            token_scope = %token_scope,
            remote_addr = %req.remote_addr().to_string(),
            version = ?req.version(),
            method = %req.method(),
            path = %req.uri().path(),
            requested_media_index = %requested_media_index,
            span.kind = "server",
            service.name = "replex",
            name = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_description = tracing::field::Empty,
        );

        async move {
            let now = Instant::now();
            ctrl.call_next(req, depot, res).await;
            let duration = now.elapsed();
            let status = res.status_code.unwrap_or(StatusCode::OK);
            match security_state(depot) {
                Some(SecurityContextState::Resolved(security)) => {
                    tracing::debug!(
                        request_id = %request_id,
                        status = %status,
                        path = %req.uri().path(),
                        username = %security.identity.username,
                        verified_uuid = %security.identity.uuid,
                        identity_source = security.identity_source.as_str(),
                        token_scope = %security.token_scope,
                        resolution_limit = ?security.policy.limit,
                        bitrate_cap = ?security.policy.max_bitrate,
                        requested_media_index = %requested_media_index,
                        platform = req.headers().get("X-Plex-Platform")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("-"),
                        product = req.headers().get("X-Plex-Product")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("-"),
                        duration = ?duration,
                        "Response"
                    );
                }
                Some(SecurityContextState::Unavailable {
                    fail_closed,
                    reason,
                }) => {
                    tracing::debug!(
                        request_id = %request_id,
                        status = %status,
                        path = %req.uri().path(),
                        token_scope = %token_scope,
                        identity_source = "unavailable",
                        identity_fail_closed = fail_closed,
                        identity_error = %reason,
                        requested_media_index = %requested_media_index,
                        duration = ?duration,
                        "Response"
                    );
                }
                None => {
                    tracing::debug!(
                        request_id = %request_id,
                        status = %status,
                        path = %req.uri().path(),
                        token_scope = %token_scope,
                        requested_media_index = %requested_media_index,
                        duration = ?duration,
                        "Response"
                    );
                }
            }
        }
        .instrument(span)
        .await
    }
}
