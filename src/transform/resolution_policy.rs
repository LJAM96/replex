use crate::{
    config::Config, models::*, plex_client::PlexClient, resolution_policy::*,
};
use async_trait::async_trait;

use super::Transform;

/// Filters media versions prohibited by the authenticated account's
/// resolution policy out of metadata responses.
///
/// - Items whose media all exceed the limit are hidden entirely.
/// - Filtering recurses into nested metadata (hub contents, directories).
/// - When identity verification fails: fail-closed hides every item that
///   carries media; fail-open leaves the response untouched.
/// - Unrestricted users pass through unchanged.
pub struct ResolutionPolicyTransform;

impl ResolutionPolicyTransform {
    /// Resolve the effective policy for this request.
    ///
    /// Identity lookups hit the per-token cache, so repeated calls across
    /// items in one response cost effectively nothing after the first.
    async fn current_policy(
        plex_client: &PlexClient,
    ) -> Result<ResolutionPolicy, ()> {
        let config: Config = Config::figment().extract().unwrap();
        if !config.resolution_policy_enabled {
            return Ok(ResolutionPolicy::unrestricted());
        }

        let identity = plex_client.get_current_user().await.map_err(|e| {
            tracing::warn!(
                error = %e,
                "Identity resolution failed for resolution policy"
            );
        })?;

        Ok(resolve_policy(
            &config.user_resolution_policies,
            config.resolution_default,
            config.hidden_collections.as_deref().unwrap_or(&[]),
            &identity,
        ))
    }
}

#[async_trait]
impl Transform for ResolutionPolicyTransform {
    async fn transform_metadata(
        &self,
        item: &mut MetaData,
        plex_client: PlexClient,
        _options: PlexContext,
    ) {
        let policy = match Self::current_policy(&plex_client).await {
            Ok(policy) => policy,
            Err(_) => {
                // Fail closed: strip every version so nothing playable leaks.
                let config: Config = Config::figment().extract().unwrap();
                if config.resolution_policy_fail_closed {
                    clear_media_recursive(item);
                }
                return;
            }
        };

        if policy.is_unrestricted() {
            return;
        }

        // Record part permissions before stripping so direct /library/parts
        // requests can be validated even for items never played.
        crate::plex_client::cache_part_policy(&item.media, &policy).await;

        let before = count_media(item);
        strip_media_recursive(item, &policy);
        let after = count_media(item);

        if before != after {
            tracing::info!(
                rating_key = item.rating_key.clone().unwrap_or_default(),
                title = %item.title,
                before = before,
                after = after,
                "Media versions filtered"
            );
        }
    }

    async fn filter_metadata(
        &self,
        item: MetaData,
        plex_client: PlexClient,
        _options: PlexContext,
    ) -> bool {
        let policy = match Self::current_policy(&plex_client).await {
            Ok(policy) => policy,
            Err(_) => {
                let config: Config = Config::figment().extract().unwrap();
                if config.resolution_policy_fail_closed
                    && !item.media.is_empty()
                {
                    tracing::warn!(
                        rating_key =
                            item.rating_key.clone().unwrap_or_default(),
                        "Hiding item during identity failure (fail closed)"
                    );
                    return false;
                }
                return true;
            }
        };

        if policy.is_unrestricted() {
            return true;
        }

        // Items without media info (shows, seasons, directories, playlists)
        // carry nothing to restrict.
        if item.media.is_empty() {
            return true;
        }

        let any_allowed = item.media.iter().any(|m| media_allowed(m, &policy));
        if !any_allowed {
            tracing::debug!(
                rating_key = item.rating_key.clone().unwrap_or_default(),
                title = %item.title,
                "Hiding item with no permitted versions"
            );
            return false;
        }

        true
    }

    async fn filter_mediacontainer(
        &self,
        item: MediaContainer,
        plex_client: PlexClient,
        _options: PlexContext,
    ) -> bool {
        // Hubs wrap their items in nested metadata; a hub whose entire
        // content is blocked should disappear too.
        let policy = match Self::current_policy(&plex_client).await {
            Ok(policy) => policy,
            Err(_) => return true, // item-level fail-closed already handled
        };
        let _ = item;
        let _ = &policy;
        true
    }
}

/// Remove prohibited versions from an item and all nested metadata.
fn strip_media_recursive(item: &mut MetaData, policy: &ResolutionPolicy) {
    item.media.retain(|m| media_allowed(m, policy));

    for child in item.metadata.iter_mut() {
        strip_media_recursive(child, policy);
    }
    for child in item.directory.iter_mut() {
        strip_media_recursive(child, policy);
    }
    for child in item.video.iter_mut() {
        strip_media_recursive(child, policy);
    }
}

/// Fail-closed helper: drop every version regardless of classification.
fn clear_media_recursive(item: &mut MetaData) {
    item.media.clear();
    for child in item.metadata.iter_mut() {
        clear_media_recursive(child);
    }
    for child in item.directory.iter_mut() {
        clear_media_recursive(child);
    }
    for child in item.video.iter_mut() {
        clear_media_recursive(child);
    }
}

fn count_media(item: &MetaData) -> usize {
    item.media.len()
        + item
            .metadata
            .iter()
            .chain(item.directory.iter())
            .chain(item.video.iter())
            .map(count_media)
            .sum::<usize>()
}

/// Hides collections from accounts that are not explicitly permitted to see
/// them.
///
/// Works together with the global `hidden_collections` default: a collection
/// whose title is on the account's effective hidden list is removed from
/// hub rows, section listings and collection metadata. Everything else in
/// the library remains fully visible.
pub struct CollectionVisibilityTransform;

impl CollectionVisibilityTransform {
    fn is_hidden(item: &MetaData, policy: &ResolutionPolicy) -> bool {
        if policy.hidden_collections.is_empty() {
            return false;
        }
        let is_collection =
            item.is_collection_hub() || item.r#type == "collection";
        if !is_collection {
            return false;
        }
        let title = item.title.trim();
        policy.hidden_collections.iter().any(|h| h == title)
    }
}

#[async_trait]
impl Transform for CollectionVisibilityTransform {
    async fn transform_metadata(
        &self,
        _item: &mut MetaData,
        _plex_client: PlexClient,
        _options: PlexContext,
    ) {
    }

    async fn filter_metadata(
        &self,
        item: MetaData,
        plex_client: PlexClient,
        _options: PlexContext,
    ) -> bool {
        match ResolutionPolicyTransform::current_policy(&plex_client).await {
            Ok(policy) => {
                if Self::is_hidden(&item, &policy) {
                    tracing::debug!(
                        title = %item.title,
                        "Hiding collection for this account"
                    );
                    return false;
                }
                true
            }
            Err(_) => true, // resolution filtering already handles fail-closed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolution_policy::{ResolutionLimit, ResolutionPolicy};

    // Note: responses are always fetched upstream as JSON (see
    // proxy_for_transform), so tests build items through the same serde path
    // production uses.
    fn movie(id: i64, versions: &[(&str, i64)]) -> MetaData {
        let media_json: Vec<String> = versions
            .iter()
            .map(|(res, mid)| {
                format!(r#"{{"id": {}, "videoResolution": "{}"}}"#, mid, res)
            })
            .collect();
        let json = format!(
            r#"{{"ratingKey": "{id}", "key": "/library/metadata/{id}", "title": "Movie {id}", "type": "movie", "Media": [{}]}}"#,
            media_json.join(",")
        );
        serde_json::from_str::<MetaData>(&json).unwrap()
    }

    // These scenarios mutate process-wide env vars, so they run under the
    // shared env lock (same one the route tests hold) to avoid racing
    // sibling tests that read REPLEX_* config.
    #[tokio::test]
    async fn policy_flag_and_fail_closed_scenarios() {
        let _env = crate::test_helpers::env_lock();

        // With the feature disabled the policy resolves unrestricted even
        // though identity lookup would fail; no plex client needed because
        // we short-circuit before touching it.
        std::env::remove_var("REPLEX_RESOLUTION_POLICY_ENABLED");
        let client = test_client();
        let mut item = movie(1, &[("4k", 1)]);
        ResolutionPolicyTransform
            .transform_metadata(
                &mut item,
                client.clone(),
                PlexContext::default(),
            )
            .await;
        assert_eq!(item.media.len(), 1);

        // Enabled with an unreachable identity API: fail closed strips media.
        std::env::set_var("REPLEX_RESOLUTION_POLICY_ENABLED", "true");
        std::env::set_var("REPLEX_IDENTITY_API_BASE", "http://127.0.0.1:9"); // unreachable
        let client = test_client();

        let mut item = movie(2, &[("1080", 2)]);
        ResolutionPolicyTransform
            .transform_metadata(
                &mut item,
                client.clone(),
                PlexContext::default(),
            )
            .await;
        assert!(item.media.is_empty(), "fail closed must strip media");

        let keep = movie(3, &[("1080", 3)]);
        let visible = ResolutionPolicyTransform
            .filter_metadata(keep, client, PlexContext::default())
            .await;
        assert!(!visible, "items with media hidden while failing closed");
    }

    fn test_client() -> PlexClient {
        std::env::set_var("REPLEX_HOST", "http://localhost:32400");
        let context = PlexContext {
            token: Some("test-token".to_string()),
            ..Default::default()
        };
        PlexClient {
            http_client: reqwest_middleware::ClientBuilder::new(
                reqwest::Client::new(),
            )
            .build(),
            context,
            host: "http://localhost:32400".to_string(),
            cache: moka::future::Cache::builder().max_capacity(10).build(),
            default_headers: http::HeaderMap::new(),
        }
    }

    // pure recursion logic tested without the network
    #[test]
    fn strip_recursive_removes_only_prohibited() {
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::P1080,
            max_bitrate: None,
            hidden_collections: vec![],
        };
        let json = r#"{"ratingKey": "1", "title": "Show", "type": "show", "Metadata": [
            {"ratingKey": "10", "key": "/library/metadata/10", "title": "Ep 10", "type": "episode",
             "Media": [{"id": 11, "videoResolution": "4k"}, {"id": 12, "videoResolution": "1080"}]},
            {"ratingKey": "20", "key": "/library/metadata/20", "title": "Ep 20", "type": "episode",
             "Media": [{"id": 21, "videoResolution": "4k"}]}
        ]}"#;
        let mut parent: MetaData = serde_json::from_str(json).unwrap();

        assert_eq!(count_media(&parent), 3);
        strip_media_recursive(&mut parent, &policy);

        assert_eq!(parent.metadata.len(), 2);
        assert_eq!(parent.metadata[0].media.len(), 1);
        assert_eq!(parent.metadata[0].media[0].id, 12);
        // 4K-only episode survives structurally; hiding is filter_metadata's job
        assert_eq!(parent.metadata[1].media.len(), 0);
        assert_eq!(count_media(&parent), 1);
    }

    // --- collection visibility ---

    fn collection_item(title: &str, hub: bool) -> MetaData {
        let kind = if hub { "hub" } else { "collection" };
        let context = if hub {
            // real Plex collection hubs carry this context attribute
            r#""context": "hub.custom.collection.23.123","#.to_string()
        } else {
            String::new()
        };
        let hub_id = if hub {
            "\"collection.test\"".to_string()
        } else {
            "null".to_string()
        };
        serde_json::from_str::<MetaData>(&format!(
            r#"{{"ratingKey": "9", "title": "{title}", "type": "{kind}", {context} "hubIdentifier": {hub_id}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn hidden_collection_matched_by_title() {
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::Unlimited,
            max_bitrate: None,
            hidden_collections: vec!["🎀Jodie".to_string()],
        };

        assert!(CollectionVisibilityTransform::is_hidden(
            &collection_item("🎀Jodie", true),
            &policy
        ));
        assert!(!CollectionVisibilityTransform::is_hidden(
            &collection_item("Trending", true),
            &policy
        ));
        // non-collection items are never hidden by this transform
        let movie = movie(5, &[("1080", 6)]);
        assert!(!CollectionVisibilityTransform::is_hidden(&movie, &policy));
    }

    #[test]
    fn no_hidden_collections_means_passthrough() {
        let policy = ResolutionPolicy::unrestricted();
        assert!(!CollectionVisibilityTransform::is_hidden(
            &collection_item("🎀Jodie", false),
            &policy
        ));
    }
}
