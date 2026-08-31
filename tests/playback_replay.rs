use replex::models::{MediaContainer, MediaContainerWrapper};
use replex::playback_selection::{best_fit_index, fallback_indexes};

#[test]
fn captured_plex_versions_keep_original_selection_indexes() {
    let captured: MediaContainerWrapper<MediaContainer> =
        serde_json::from_str(include_str!("fixtures/playback_versions.json"))
            .expect("captured Plex payload remains compatible");
    let media = &captured.media_container.metadata[0].media;

    assert_eq!(best_fit_index(media, (1920, 1080)), Some(2));
    assert_eq!(fallback_indexes(media, 0, "4k", None), vec![2, 1]);
}
