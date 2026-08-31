use crate::{models::*, plex_client::PlexClient};
use async_trait::async_trait;

use super::Transform;
#[derive(Default)]
pub struct HubRestrictionTransform;

#[async_trait]
impl Transform for HubRestrictionTransform {
    async fn filter_metadata(
        &self,
        item: &MetaData,
        plex_client: PlexClient,
        _options: PlexContext,
    ) -> bool {
        let config = &plex_client.config;

        if !config.hub_restrictions {
            return true;
        }

        if item.is_hub() && !item.is_collection_hub() {
            return true;
        }

        if !item.is_hub() {
            return true;
        }

        if item.size.unwrap_or_default() == 0 {
            return false;
        }

        let section_id = match item.library_section_id.or_else(|| {
            item.hub_identifier.as_deref().and_then(|identifier| {
                identifier.split('.').nth(2)?.parse::<i64>().ok()
            })
        }) {
            Some(id) => id,
            None => {
                tracing::warn!(
                    hub_identifier = ?item.hub_identifier,
                    "Ignoring malformed collection hub without a section id"
                );
                return true;
            }
        };

        //let start = Instant::now();
        let mut custom_collections = match plex_client
            .clone()
            .get_cached(
                plex_client.get_section_collections(section_id),
                format!("sectioncollections:{}", section_id),
            )
            .await
        {
            Ok(collections) => collections,
            Err(error) => {
                tracing::warn!(error = %error, section_id, "Could not load custom collections for hub restriction");
                return true;
            }
        };

        //println!("Elapsed time: {:.2?}", start.elapsed());
        let custom_collections_ids: Vec<String> = custom_collections
            .media_container
            .children()
            .iter()
            .filter_map(|c| c.rating_key.clone())
            .collect();

        item.hub_identifier
            .as_deref()
            .and_then(|identifier| identifier.split('.').next_back())
            .map(|id| {
                custom_collections_ids
                    .iter()
                    .any(|candidate| candidate == id)
            })
            .unwrap_or(true)
    }
}
