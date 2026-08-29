use super::hero_meta;
use super::MediaStyleTransform;
use super::Transform;
use crate::{models::*, plex_client::PlexClient};
use async_trait::async_trait;
use futures_util::{stream::FuturesOrdered, StreamExt};

#[derive(Default, Debug)]
pub struct HubStyleTransform {
    pub is_home: bool, // use clip instead of hero for android
}

pub struct ClientHeroStyle {
    pub enabled: bool,
    pub include_meta: bool,
    pub r#type: String,
    pub style: Option<String>,
    pub child_type: Option<String>,
    pub cover_art_as_thumb: bool, // if we should return the coverart in the thumb field
    pub cover_art_as_art: bool, // if we should return the coverart in the art field
}

impl Default for ClientHeroStyle {
    fn default() -> Self {
        Self {
            enabled: true,
            include_meta: true,
            style: Some("hero".to_string()),
            r#type: "mixed".to_string(),
            child_type: None,
            cover_art_as_thumb: true,
            cover_art_as_art: false,
        }
    }
}

#[derive(Debug)]
pub enum DeviceType {
    Tv,
    Mobile,
}

impl DeviceType {
    pub fn from_context(context: &PlexContext) -> DeviceType {
        let product =
            context.product.clone().unwrap_or_default().to_lowercase();
        let device = context.device.clone().unwrap_or_default().to_lowercase();
        let model = context.model.clone().unwrap_or_default().to_lowercase();

        if product.contains("(tv)")
            || product.contains("apple tv")
            || product.contains("android tv")
            || device.contains("apple tv")
            || model.contains("appletv")
        {
            DeviceType::Tv
        } else {
            DeviceType::Mobile
        }
    }
}

impl ClientHeroStyle {
    pub fn from_context(context: PlexContext) -> Self {
        // pub fn android(product: String, platform_version: String) -> Self {
        let product = context.product.clone().unwrap_or_default();
        let device_type = DeviceType::from_context(&context);
        let platform = context.platform.clone().unwrap_or_default();
        let platform_version =
            context.platform_version.clone().unwrap_or_default();

        match platform {
            Platform::Android => {
                match device_type {
                    DeviceType::Tv => {
                        //   dbg!(context);
                        Self {
                            style: Some("hero".to_string()),
                            // clip wil make the item info disappear on TV
                            r#type: "clip".to_string(),
                            // using clip makes it load thumbs instead of art as cover art. So we don't have to touch the background
                            child_type: Some("clip".to_string()),
                            cover_art_as_art: true, // Home doesn't work correctly without.
                            cover_art_as_thumb: true,
                            ..ClientHeroStyle::default()
                        }
                    }
                    _ => Self {
                        style: None,
                        r#type: "clip".to_string(),
                        child_type: Some("clip".to_string()),
                        cover_art_as_art: true,
                        ..ClientHeroStyle::default()
                    },
                }
            }
            Platform::Roku => ClientHeroStyle::roku(),
            Platform::Ios => ClientHeroStyle::ios_style(),
            Platform::TvOS => ClientHeroStyle::tvos_style(),
            _ => {
                if product.clone().to_lowercase() == "plex web" {
                    ClientHeroStyle::web()
                } else {
                    ClientHeroStyle::default()
                }
            } // _ => {
              //     if product.starts_with("Plex HTPC") {
              //         ClientHeroStyle::htpc_style()
              //     } else {
              //         match product.to_lowercase().as_ref() {
              //             "plex for lg" => ClientHeroStyle::htpc_style(),
              //             "plex for xbox" => ClientHeroStyle::htpc_style(),
              //             "plex for ps4" => ClientHeroStyle::htpc_style(),
              //             "plex for ps5" => ClientHeroStyle::htpc_style(),
              //             "plex for ios" => ClientHeroStyle::ios_style(),
              //             _ => ClientHeroStyle::default(),
              //         }
              //     }
              // }
        }
    }

    pub fn roku() -> Self {
        Self {
            style: Some("hero".to_string()),
            ..ClientHeroStyle::default()
        }
    }

    pub fn web() -> Self {
        Self {
            // Plex Web's hero renderer requires the hub Meta block
            // (displayFields/displayImages); without it the row silently
            // degrades to a poster shelf.
            include_meta: true,
            cover_art_as_art: true,
            cover_art_as_thumb: true,
            ..ClientHeroStyle::default()
        }
    }

    pub fn htpc_style() -> Self {
        Self {
            ..ClientHeroStyle::default()
        }
    }

    pub fn ios_style() -> Self {
        Self {
            cover_art_as_art: true,
            cover_art_as_thumb: false, // ios doesnt load the subview as hero.
            ..ClientHeroStyle::default()
        }
    }

    pub fn tvos_style() -> Self {
        Self {
            cover_art_as_art: true,
            cover_art_as_thumb: false, // ios doesnt load the subview as hero.
            ..ClientHeroStyle::default()
        }
    }

    // pub fn for_client(platform: Platform, product: String, platform_version: String) -> Self {
    //     match platform {
    //         Platform::Android => PlatformHeroStyle::android(product, platform_version),
    //         Platform::Roku => PlatformHeroStyle::roku(product),
    //         _ => {
    //             if product.starts_with("Plex HTPC") {
    //               ClientHeroStyle::htpc_style()
    //             } else {
    //                 match product.to_lowercase().as_ref() {
    //                     "plex for lg" => ClientHeroStyle::htpc_style(),
    //                     "plex for xbox" => ClientHeroStyle::htpc_style(),
    //                     "plex for ps4" => ClientHeroStyle::htpc_style(),
    //                     "plex for ps5" => ClientHeroStyle::htpc_style(),
    //                     "plex for ios" => ClientHeroStyle::ios_style(),
    //                     _ => ClientHeroStyle::default(),
    //                 }
    //             }
    //         }
    //     }
    // }
}

#[async_trait]
impl Transform for HubStyleTransform {
    async fn transform_metadata(
        &self,
        item: &mut MetaData,
        plex_client: PlexClient,
        options: PlexContext,
    ) {
        let style = item.style.clone().unwrap_or("".to_string()).to_owned();

        if item.is_hub() {
            // TODO: Check why tries to load non existing collectiin? my guess is no access
            let is_hero =
                item.is_hero(plex_client.clone()).await.unwrap_or(false);

            if is_hero {
                let style = ClientHeroStyle::from_context(options.clone());

                item.style = style.style;

                item.r#type = style.r#type;

                if style.include_meta {
                    item.meta = Some(hero_meta());
                }

                let mut futures = FuturesOrdered::new();
                // let now = Instant::now();

                for mut child in item.children() {
                    if let Some(child_type) = style.child_type.clone() {
                        child.r#type = child_type;
                    }

                    let client = plex_client.clone();
                    let _options = options.clone();
                    futures.push_back(async move {
                        let mut c = child.clone();
                        let transform =
                            MediaStyleTransform { style: Style::Hero };
                        transform
                            .transform_metadata(&mut c, client, _options)
                            .await;
                        c
                    });
                }
                let children: Vec<MetaData> = futures.collect().await;
                item.set_children(children);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientHeroStyle, Platform, PlexContext};

    fn context(platform: Platform, product: &str) -> PlexContext {
        PlexContext {
            platform: Some(platform),
            product: Some(product.to_string()),
            ..PlexContext::default()
        }
    }

    #[test]
    fn preview_client_hero_profiles_keep_expected_artwork_fallbacks() {
        let web = ClientHeroStyle::from_context(context(
            Platform::Chrome,
            "Plex Web",
        ));
        assert_eq!(web.style.as_deref(), Some("hero"));
        assert!(web.include_meta);
        assert!(web.cover_art_as_thumb);
        assert!(web.cover_art_as_art);

        let ios = ClientHeroStyle::from_context(context(
            Platform::Ios,
            "Plex for iOS",
        ));
        assert_eq!(ios.style.as_deref(), Some("hero"));
        assert!(ios.include_meta);
        assert!(!ios.cover_art_as_thumb);
        assert!(ios.cover_art_as_art);

        let tvos = ClientHeroStyle::from_context(context(
            Platform::TvOS,
            "Plex for Apple TV",
        ));
        assert_eq!(tvos.style.as_deref(), Some("hero"));
        assert!(tvos.include_meta);
        assert!(!tvos.cover_art_as_thumb);
        assert!(tvos.cover_art_as_art);

        let roku = ClientHeroStyle::from_context(context(
            Platform::Roku,
            "Plex for Roku",
        ));
        assert_eq!(roku.style.as_deref(), Some("hero"));
        assert!(roku.include_meta);
        assert!(roku.cover_art_as_thumb);
        assert!(!roku.cover_art_as_art);

        let android_tv = ClientHeroStyle::from_context(context(
            Platform::Android,
            "Plex for Android (TV)",
        ));
        assert_eq!(android_tv.style.as_deref(), Some("hero"));
        assert_eq!(android_tv.child_type.as_deref(), Some("clip"));
        assert!(android_tv.include_meta);
        assert!(android_tv.cover_art_as_thumb);
        assert!(android_tv.cover_art_as_art);
    }

    #[test]
    fn continue_watching_golden_fixture_remains_a_hero_hub() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/mock/out/hubs_promoted_6.json"
        ))
        .expect("promoted hubs fixture should be valid JSON");
        let hubs = fixture["MediaContainer"]["Hub"]
            .as_array()
            .expect("promoted hubs fixture should contain hubs");
        let continue_watching = hubs
            .iter()
            .find(|hub| hub["hubIdentifier"] == "home.continue")
            .expect("fixture should contain Continue Watching");

        assert_eq!(continue_watching["style"], "hero");
        assert!(continue_watching["Meta"].is_object());
        let first_child = &continue_watching["Metadata"][0];
        assert!(first_child["Image"].as_array().is_some_and(|images| images
            .iter()
            .any(|image| image["type"] == "coverArt")));
    }
}
