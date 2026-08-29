pub mod collection_style;
pub mod hub_interleave;
pub mod hub_key;
pub mod hub_limit;
pub mod hub_section_directory;
pub mod hub_style;
pub mod hub_watched;
pub mod library_interleave;
pub mod media_style;
pub mod resolution_policy;
pub mod restrictions;
pub mod user_state;

pub use collection_style::CollectionStyleTransform;
pub use hub_interleave::HubInterleaveTransform;
pub use hub_key::HubKeyTransform;
pub use hub_limit::HubChildrenLimitTransform;
pub use hub_section_directory::HubSectionDirectoryTransform;
pub use hub_style::{ClientHeroStyle, HubStyleTransform};
pub use hub_watched::HubWatchedTransform;
pub use library_interleave::LibraryInterleaveTransform;
pub use media_style::MediaStyleTransform;
pub use resolution_policy::{
    CollectionVisibilityTransform, ResolutionPolicyTransform,
};
pub use restrictions::HubRestrictionTransform;
pub use user_state::UserStateTransform;

use crate::{models::*, plex_client::PlexClient};

use async_recursion::async_recursion;
use async_trait::async_trait;
use futures_util::{
    future::{self},
    StreamExt,
};
use std::sync::Arc;

#[async_trait]
pub trait Transform: Send + Sync + 'static {
    async fn transform_metadata(
        &self,
        item: &mut MetaData,
        //item: MetaData,
        plex_client: PlexClient,
        options: PlexContext,
        //) -> Option<MediaContainer> {
    ) {
    }
    async fn transform_mediacontainer(
        &self,
        item: MediaContainer,
        plex_client: PlexClient,
        options: PlexContext,
    ) -> MediaContainer {
        item
    }
    async fn filter_metadata(
        &self,
        item: &MetaData,
        plex_client: PlexClient,
        options: PlexContext,
    ) -> bool {
        true
    }
}

// #[derive(TypedBuilder)]
// #[derive(Default)]
#[derive(Clone)]
pub struct TransformBuilder {
    pub plex_client: PlexClient,
    pub options: PlexContext,
    pub mix: bool,
    pub transforms: Vec<Arc<dyn Transform>>,
}

impl TransformBuilder {
    #[inline]
    pub fn new(plex_client: PlexClient, options: PlexContext) -> Self {
        Self {
            transforms: Vec::new(),
            mix: false,
            plex_client,
            options,
        }
    }

    #[inline]
    pub fn with_transform<T: Transform>(mut self, transform: T) -> Self {
        self.transforms.push(Arc::new(transform));
        // self.transforms.insert(0, Arc::new(transform));
        self
    }

    pub async fn apply_to_old(
        self,
        container: &mut MediaContainerWrapper<MediaContainer>,
    ) {
        let children = container.media_container.children_mut();
        //let new_children = self.apply_to_metadata(children).await;
        //container.media_container.set_children(new_children);

        for t in self.transforms.clone() {
            let futures =
                container.media_container.children_mut().iter_mut().map(
                    |x: &mut MetaData| {
                        t.transform_metadata(
                            x,
                            self.plex_client.clone(),
                            self.options.clone(),
                        )
                    },
                );

            future::join_all(futures).await;
            //for k in container.media_container.children_mut()

            // dont use join as it needs ti be executed in order
            // let l = std::cell::RefCell::new(&mut container.media_container);
            container.media_container = t
                .transform_mediacontainer(
                    container.media_container.clone(),
                    self.plex_client.clone(),
                    self.options.clone(),
                )
                .await;
            // dbg!(container.media_container.size);
        }

        if container.media_container.size.is_some() {
            container.media_container.size = Some(
                container
                    .media_container
                    .children_mut()
                    .len()
                    .try_into()
                    .unwrap(),
            );
        }
    }

    pub async fn apply_to(
        self,
        container: &mut MediaContainerWrapper<MediaContainer>,
    ) {
        for t in self.transforms.clone() {
            // Filter by index rather than optional Plex keys. Some legitimate
            // metadata nodes do not carry `key`; key-based removal both cloned
            // every item and could panic on those responses.
            let mut index = 0usize;
            while index < container.media_container.children_mut().len() {
                let keep = t
                    .filter_metadata(
                        &container.media_container.children_mut()[index],
                        self.plex_client.clone(),
                        self.options.clone(),
                    )
                    .await;
                if !keep {
                    container.media_container.children_mut().remove(index);
                    continue;
                }
                t.transform_metadata(
                    &mut container.media_container.children_mut()[index],
                    self.plex_client.clone(),
                    self.options.clone(),
                )
                .await;
                index += 1;
            }

            container.media_container = t
                .transform_mediacontainer(
                    container.media_container.clone(),
                    self.plex_client.clone(),
                    self.options.clone(),
                )
                .await;
        }

        if container.media_container.size.is_some() {
            container.media_container.size = Some(
                i64::try_from(container.media_container.children_mut().len())
                    .unwrap_or(i64::MAX),
            );
        }
    }
}

pub fn hero_meta() -> Meta {
    Meta {
        r#type: None,
        // r#type: Some(MetaType {
        //     active: Some(true),
        //     r#type: Some("mixed".to_string()),
        //     title: Some("All".to_string()),
        // }),
        // style: Some(Style::Hero.to_string().to_lowercase()),
        // display_fields: vec![],
        display_fields: vec![
            DisplayField {
                r#type: Some("movie".to_string()),
                fields: vec![
                    "title".to_string(),
                    "originallyAvailableAt".to_string(),
                ],
            },
            DisplayField {
                r#type: Some("show".to_string()),
                fields: vec!["title".to_string(), "childCount".to_string()],
            },
            DisplayField {
                r#type: Some("clip".to_string()),
                fields: vec![
                    "title".to_string(),
                    "originallyAvailableAt".to_string(),
                ],
            },
            DisplayField {
                r#type: Some("mixed".to_string()),
                fields: vec![
                    "title".to_string(),
                    "originallyAvailableAt".to_string(),
                ],
            },
        ],
        display_images: vec![
            DisplayImage {
                r#type: Some("hero".to_string()),
                image_type: Some("coverArt".to_string()),
            },
            DisplayImage {
                r#type: Some("mixed".to_string()),
                image_type: Some("coverArt".to_string()),
            },
            DisplayImage {
                r#type: Some("clip".to_string()),
                image_type: Some("coverArt".to_string()),
            },
            DisplayImage {
                r#type: Some("movie".to_string()),
                image_type: Some("coverArt".to_string()),
            },
            DisplayImage {
                r#type: Some("show".to_string()),
                image_type: Some("coverArt".to_string()),
            },
        ],
    }
}
