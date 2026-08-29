use crate::{models::*, plex_client::PlexClient};

use super::hero_meta;
use super::ClientHeroStyle;
use super::MediaStyleTransform;
use super::Transform;
use async_trait::async_trait;
use futures_util::{stream::FuturesOrdered, StreamExt};

/// Collections can be called from hubs as a refresh. But also standalone.
/// We need to know if if its hub called and if the hub is hero styled for media.
#[derive(Default, Debug)]
pub struct CollectionStyleTransform {
    pub collection_ids: Vec<u32>,
    pub hub: bool, // if collections is meant for hubs
}

#[async_trait]
impl Transform for CollectionStyleTransform {
    async fn transform_mediacontainer(
        &self,
        mut item: MediaContainer,
        plex_client: PlexClient,
        options: PlexContext,
    ) -> MediaContainer {
        let Some(collection_id) = self.collection_ids.first().copied() else {
            return item;
        };
        let collection_details = plex_client
            .clone()
            .get_cached(
                plex_client.get_collection(collection_id as i32),
                format!("collection:{collection_id}"),
            )
            .await;

        let is_hero = collection_details
            .ok()
            .and_then(|mut details| {
                details.media_container.children().first().cloned()
            })
            .is_some_and(|metadata| {
                metadata.has_label("REPLEXHERO".to_string())
            });

        if is_hero {
            // let mut futures = FuturesOrdered::new();
            // let now = Instant::now();

            let style = ClientHeroStyle::from_context(options.clone());

            item.meta = Some(hero_meta());

            let mut futures = FuturesOrdered::new();
            for mut child in item.children() {
                if let Some(child_type) = style.child_type.clone() {
                    child.r#type = child_type;
                }

                let client = plex_client.clone();
                let _options = options.clone();
                futures.push_back(async move {
                    let mut c = child.clone();
                    let transform = MediaStyleTransform { style: Style::Hero };
                    transform
                        .transform_metadata(&mut c, client, _options)
                        .await;
                    c
                });
            }
            let children: Vec<MetaData> = futures.collect().await;
            item.set_children(children);
        }
        item
    }
}
