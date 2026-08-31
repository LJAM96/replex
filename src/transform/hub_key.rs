use crate::{models::*, plex_client::PlexClient};

use super::Transform;
use async_trait::async_trait;

#[derive(Default, Debug)]
pub struct HubKeyTransform;

/// We point to replex so we can do some transform on the children calls
#[async_trait]
impl Transform for HubKeyTransform {
    async fn transform_metadata(
        &self,
        item: &mut MetaData,
        _plex_client: PlexClient,
        _options: PlexContext,
    ) {
        if item.is_hub()
            && item
                .key
                .as_deref()
                .is_some_and(|key| !key.starts_with("/replex"))
        {
            // might already been set by the mixings
            // setting an url argument crashes client. So we use the path
            let Some(old_key) = item.key.clone() else {
                return;
            };
            item.key = Some(format!(
                "/replex/{}{}",
                item.style
                    .clone()
                    .unwrap_or(Style::Shelf.to_string().to_lowercase()),
                old_key
            ));
            tracing::debug!(
                old_key = old_key,
                key = &item.key,
                "Replacing hub key"
            );
        }
    }
}
