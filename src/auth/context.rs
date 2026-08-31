use crate::models::PlexContext;
use crate::plex_client::PlexClient;
use crate::resolution_policy::{
    resolve_policy, IdentitySource, ResolutionPolicy, UserIdentity,
};
use crate::state;
use data_encoding::BASE32;
use salvo::{async_trait, Depot, FlowCtrl, Handler, Request, Response};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const REQUEST_CONTEXT_KEY: &str = "replex.request_context";
const SECURITY_CONTEXT_KEY: &str = "replex.security_context";
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub plex: PlexContext,
    pub upstream_host: String,
    pub request_id: String,
    pub token_scope: String,
}

#[derive(Debug, Clone)]
pub struct RequestSecurityContext {
    pub identity: UserIdentity,
    pub token_scope: String,
    pub policy: ResolutionPolicy,
    pub identity_source: IdentitySource,
}

#[derive(Debug, Clone)]
pub enum SecurityContextState {
    Resolved(Arc<RequestSecurityContext>),
    Unavailable { fail_closed: bool, reason: String },
}

pub fn request_context(depot: &Depot) -> anyhow::Result<Arc<RequestContext>> {
    depot
        .get::<Arc<RequestContext>>(REQUEST_CONTEXT_KEY)
        .cloned()
        .map_err(|_| anyhow::anyhow!("request context missing"))
}

pub fn security_state(depot: &Depot) -> Option<SecurityContextState> {
    depot
        .get::<SecurityContextState>(SECURITY_CONTEXT_KEY)
        .ok()
        .cloned()
}

fn decoded_replex_stream_host(req: &Request) -> Option<String> {
    let host = req
        .headers()
        .get("HOST")
        .and_then(|value| value.to_str().ok())?;
    let encoded = host.strip_suffix(".replex.stream")?;
    if encoded.is_empty() {
        return None;
    }
    let encoded = encoded.to_ascii_uppercase();
    let decoded_len = BASE32.decode_len(encoded.len()).ok()?;
    let mut output = vec![0u8; decoded_len];
    let len = BASE32.decode_mut(encoded.as_bytes(), &mut output).ok()?;
    if len == 0 {
        return None;
    }
    let value = std::str::from_utf8(&output[..len]).ok()?.trim();
    if !(value.starts_with("http://") || value.starts_with("https://")) {
        return None;
    }
    let parsed = url::Url::parse(value).ok()?;
    if parsed.host_str().is_none()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    Some(value.trim_end_matches('/').to_string())
}

fn requires_security_context(path: &str) -> bool {
    path.starts_with("/library/")
        || path.starts_with("/hubs/")
        || path == "/hubs"
        || path.starts_with("/playQueues")
        || path.starts_with("/video/")
}

#[derive(Clone)]
pub struct RequestContextMiddleware;

#[async_trait]
impl Handler for RequestContextMiddleware {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let state = match state::from_depot(depot) {
            Ok(state) => state,
            Err(error) => {
                tracing::error!(error = %error, "request context missing app state");
                res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
                ctrl.skip_rest();
                return;
            }
        };

        let plex = req.extract::<PlexContext>().await.unwrap_or_default();
        let decoded_host = decoded_replex_stream_host(req);
        if let Some(ref candidate) = decoded_host {
            if !state.config.allows_upstream_host(candidate) {
                tracing::warn!(
                    request_host = %req.headers().get("HOST")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("invalid"),
                    "rejected unapproved dynamic Plex upstream"
                );
                res.status_code(salvo::http::StatusCode::BAD_REQUEST);
                ctrl.skip_rest();
                return;
            }
        }
        let upstream_host = decoded_host
            .or_else(|| state.config.host.clone())
            .unwrap_or_default();
        let request_id = format!(
            "r{:016x}",
            REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let context = Arc::new(RequestContext {
            token_scope: crate::account_scope::token_scope(
                plex.token.as_deref(),
            ),
            plex,
            upstream_host,
            request_id,
        });
        let _ = depot.insert(REQUEST_CONTEXT_KEY, context);
        ctrl.call_next(req, depot, res).await;
    }
}

#[derive(Clone)]
pub struct SecurityContextMiddleware;

#[async_trait]
impl Handler for SecurityContextMiddleware {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let state = match state::from_depot(depot) {
            Ok(state) => state,
            Err(error) => {
                tracing::error!(error = %error, "security context missing app state");
                res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
                ctrl.skip_rest();
                return;
            }
        };
        let request = match request_context(depot) {
            Ok(request) => request,
            Err(error) => {
                tracing::error!(error = %error, "security context missing request context");
                res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
                ctrl.skip_rest();
                return;
            }
        };

        let runtime_policy = state.policy.snapshot().await;
        if runtime_policy.enabled && requires_security_context(req.uri().path())
        {
            let result =
                PlexClient::from_context_with_state(&request.plex, &state).map(
                    |mut client| {
                        client.host = request.upstream_host.clone();
                        client
                    },
                );
            let security = match result {
                Ok(client) => match client.get_current_identity().await {
                    Ok(resolved) => {
                        let policy = resolve_policy(
                            &runtime_policy.entries,
                            runtime_policy.default_limit,
                            &runtime_policy.hidden_collections,
                            &resolved.identity,
                        );
                        SecurityContextState::Resolved(Arc::new(
                            RequestSecurityContext {
                                identity: resolved.identity,
                                token_scope: request.token_scope.clone(),
                                policy,
                                identity_source: resolved.source,
                            },
                        ))
                    }
                    Err(error) => SecurityContextState::Unavailable {
                        fail_closed: runtime_policy.fail_closed,
                        reason: error.to_string(),
                    },
                },
                Err(error) => SecurityContextState::Unavailable {
                    fail_closed: runtime_policy.fail_closed,
                    reason: error.to_string(),
                },
            };
            let _ = depot.insert(SECURITY_CONTEXT_KEY, security);
        }

        ctrl.call_next(req, depot, res).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_path_classification_covers_policy_surfaces() {
        assert!(requires_security_context("/library/metadata/1"));
        assert!(requires_security_context("/hubs/home"));
        assert!(requires_security_context("/playQueues"));
        assert!(requires_security_context(
            "/video/:/transcode/universal/decision"
        ));
        assert!(!requires_security_context("/web/index.html"));
        assert!(!requires_security_context("/ping"));
    }

    #[test]
    fn token_scope_never_contains_the_token() {
        let token = "secret-plex-token";
        let scope = crate::account_scope::token_scope(Some(token));
        assert_eq!(scope.len(), 16);
        assert!(!scope.contains(token));
    }

    #[test]
    fn encoded_dynamic_host_decodes_only_to_a_candidate() {
        let candidate = "http://attacker.invalid";
        let encoded = BASE32.encode(candidate.as_bytes()).to_ascii_lowercase();
        let mut request = Request::new();
        request.headers_mut().insert(
            "HOST",
            format!("{encoded}.replex.stream").parse().unwrap(),
        );
        assert_eq!(
            decoded_replex_stream_host(&request).as_deref(),
            Some(candidate)
        );
        assert!(upstream_candidate_needs_configuration(candidate));
    }

    fn upstream_candidate_needs_configuration(candidate: &str) -> bool {
        url::Url::parse(candidate).is_ok()
    }
}
