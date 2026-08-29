use super::hero_meta;
use super::ClientHeroStyle;
use super::Transform;
use crate::{models::*, plex_client::PlexClient};
use async_trait::async_trait;

pub struct MediaStyleTransform {
    pub style: Style,
}

fn upsert_cover_art(images: &mut Vec<Image>, title: &str, cover_art: &str) {
    if let Some(image) =
        images.iter_mut().find(|image| image.r#type == "coverArt")
    {
        image.url = cover_art.to_string();
        image.alt = Some(title.to_string());
        return;
    }

    images.push(Image {
        r#type: "coverArt".to_string(),
        url: cover_art.to_string(),
        alt: Some(title.to_string()),
    });
}

#[async_trait]
impl Transform for MediaStyleTransform {
    async fn transform_mediacontainer(
        &self,
        mut item: MediaContainer,
        plex_client: PlexClient,
        options: PlexContext,
    ) -> MediaContainer {
        if self.style == Style::Hero {
            item.meta = Some(hero_meta());
        }
        item
    }

    async fn transform_metadata(
        &self,
        item: &mut MetaData,
        plex_client: PlexClient,
        options: PlexContext,
    ) {
        if self.style == Style::Hero {
            let style_def = ClientHeroStyle::from_context(options.clone());
            if let Some(child_type) = style_def.child_type.clone() {
                item.r#type = child_type;
            }

            let cover_art = if let Some(custom_url) =
                item.get_label_value("REPLEXHEROURL")
            {
                Some(custom_url)
            } else {
                let Some(mut guid) = item.guid.clone() else {
                    return;
                };
                if guid.starts_with("plex://episode") {
                    if let Some(parent_guid) = item.parent_guid.clone() {
                        guid = parent_guid;
                    }
                }
                guid = guid.replace("plex://", "");

                let Some(token) = options.token.as_deref() else {
                    return;
                };
                let host = options
                    .forwarded_host
                    .as_deref()
                    .or(options.host.as_deref());
                let Some(host) = host else {
                    return;
                };

                Some(format!(
                    "{}://{}/replex/image/hero/{}?X-Plex-Token={}",
                    options.forwarded_proto.as_deref().unwrap_or("http"),
                    host,
                    guid,
                    token
                ))
            };
            //dbg!(&cover_art);
            if let Some(cover_art) = cover_art {
                // Modern Plex Experience clients make their own artwork
                // choices from the complete Image list. Preserve Plex's
                // native coverPoster/background/clearLogo/etc. entries and
                // only add or replace the coverArt entry Replex owns.
                upsert_cover_art(&mut item.images, &item.title, &cover_art);
                // lots of clients dont listen to the above
                if style_def.cover_art_as_art {
                    item.art = Some(cover_art.clone());
                }

                if style_def.cover_art_as_thumb {
                    item.thumb = Some(cover_art);
                }
            }
        }
        // item
    }
}

#[cfg(test)]
mod tests {
    use super::upsert_cover_art;
    use crate::models::Image;

    #[test]
    fn hero_cover_art_preserves_native_plex_images() {
        let mut images = vec![
            Image {
                r#type: "coverPoster".to_string(),
                url: "/poster".to_string(),
                alt: Some("Example".to_string()),
            },
            Image {
                r#type: "background".to_string(),
                url: "/background".to_string(),
                alt: Some("Example".to_string()),
            },
            Image {
                r#type: "clearLogo".to_string(),
                url: "/logo".to_string(),
                alt: Some("Example".to_string()),
            },
        ];

        upsert_cover_art(&mut images, "Example", "/replex/hero");

        assert_eq!(images.len(), 4);
        assert!(images.iter().any(
            |image| image.r#type == "coverPoster" && image.url == "/poster"
        ));
        assert!(images
            .iter()
            .any(|image| image.r#type == "background"
                && image.url == "/background"));
        assert!(images
            .iter()
            .any(|image| image.r#type == "clearLogo" && image.url == "/logo"));
        assert!(images
            .iter()
            .any(|image| image.r#type == "coverArt"
                && image.url == "/replex/hero"));
    }

    #[test]
    fn hero_cover_art_replaces_existing_cover_art_without_duplicates() {
        let mut images = vec![
            Image {
                r#type: "coverArt".to_string(),
                url: "/old-hero".to_string(),
                alt: Some("Old".to_string()),
            },
            Image {
                r#type: "backgroundSquare".to_string(),
                url: "/square".to_string(),
                alt: Some("Example".to_string()),
            },
        ];

        upsert_cover_art(&mut images, "Example", "/new-hero");

        assert_eq!(
            images
                .iter()
                .filter(|image| image.r#type == "coverArt")
                .count(),
            1
        );
        let cover_art = images
            .iter()
            .find(|image| image.r#type == "coverArt")
            .expect("coverArt should exist");
        assert_eq!(cover_art.url, "/new-hero");
        assert_eq!(cover_art.alt.as_deref(), Some("Example"));
        assert!(images.iter().any(|image| image.r#type == "backgroundSquare"
            && image.url == "/square"));
    }
}
