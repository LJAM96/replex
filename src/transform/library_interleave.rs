use super::Transform;
use crate::{models::*, plex_client::PlexClient};
use async_trait::async_trait;
use itertools::Itertools;

#[derive(Default, Debug, Clone)]
pub struct LibraryInterleaveTransform {
    pub collection_ids: Vec<u32>,
    pub offset: i32,
    pub limit: i32,
}

#[async_trait]
impl Transform for LibraryInterleaveTransform {
    async fn transform_mediacontainer(
        &self,
        mut item: MediaContainer,
        plex_client: PlexClient,
        _options: PlexContext,
    ) -> MediaContainer {
        let config = &plex_client.config;
        if !config.interleave {
            return item;
        }
        let mut children: Vec<MetaData> = vec![];
        let mut total_size = 0;

        for id in self.collection_ids.clone() {
            let collection = match plex_client
                .clone()
                .get_cached(
                    plex_client.get_collection(id as i32),
                    format!("collection:{id}"),
                )
                .await
            {
                Ok(collection) => collection,
                Err(error) => {
                    tracing::warn!(collection_id = id, error = %error, "Skipping inaccessible collection during library interleave");
                    continue;
                }
            };

            //match c {
            //    Ok(v) =>,
            //    Err(err) =>
            //}

            let mut c = match plex_client
                .clone()
                .get_cached(
                    plex_client.get_collection_children(
                        id as i64,
                        Some(self.offset),
                        Some(self.limit),
                    ),
                    format!(
                        "get_collection_children:{}:{}:{}",
                        id, self.offset, self.limit
                    ),
                )
                .await
            {
                Ok(children) => children,
                Err(error) => {
                    tracing::warn!(collection_id = id, error = %error, "Skipping collection children during library interleave");
                    continue;
                }
            };

            // should have proper errors but lets assume not found so no access
            //match c {
            //    Ok(v) =>,
            //    Err(err) =>
            //}

            let collection_excludes_watched = config.exclude_watched
                || collection.media_container.metadata.first().is_some_and(
                    |metadata| {
                        metadata.has_label("REPLEX_EXCLUDE_WATCHED".to_string())
                    },
                );
            if collection_excludes_watched {
                c.media_container.children_mut().retain(|x| !x.is_watched());
            }

            total_size += c.media_container.children().len() as i32;

            match children.is_empty() {
                false => {
                    children = children
                        .into_iter()
                        .interleave(c.media_container.children())
                        .collect::<Vec<MetaData>>();
                }
                true => children.append(&mut c.media_container.children()),
            }
        }
        item.total_size = Some(total_size);
        // always metadata
        item.metadata = children;
        item
    }
}
