use crate::models::{Media, MediaContainer, MediaContainerWrapper};
use crate::resolution_policy::{media_allowed, ResolutionPolicy};

/// Parse Plex's optional media index. `-1` and absence mean the first item.
pub fn requested_media_index(value: Option<&str>) -> Option<usize> {
    match value {
        None | Some("-1") => Some(0),
        Some(value) => value.parse().ok(),
    }
}

fn density(media: &Media) -> Option<u128> {
    let width = u128::try_from(media.width?).ok()?;
    let height = u128::try_from(media.height?).ok()?;
    width.checked_mul(height)
}

/// Original Plex index of the closest version to a device's screen. Missing
/// dimensions sort last; ties prefer the higher-resolution source.
pub fn best_fit_index(media: &[Media], screen: (i64, i64)) -> Option<usize> {
    let screen_density = u128::try_from(screen.0)
        .ok()?
        .checked_mul(u128::try_from(screen.1).ok()?)?;
    media
        .iter()
        .enumerate()
        .min_by_key(|(_, item)| match density(item) {
            Some(value) => {
                (0, value.abs_diff(screen_density), u128::MAX - value)
            }
            None => (1, u128::MAX, u128::MAX),
        })
        .map(|(index, _)| index)
}

/// Eligible fallback versions as their original Plex indexes, highest known
/// resolution first. Keeping original indexes prevents sorting/filtering from
/// silently selecting a different source version.
pub fn fallback_indexes(
    media: &[Media],
    selected_index: usize,
    fallback_resolution: &str,
    policy: Option<&ResolutionPolicy>,
) -> Vec<usize> {
    let mut candidates: Vec<usize> = media
        .iter()
        .enumerate()
        .filter(|(index, item)| {
            *index != selected_index
                && item.video_resolution.as_deref().is_some_and(|value| {
                    !value.eq_ignore_ascii_case(fallback_resolution)
                })
                && policy.is_none_or(|policy| media_allowed(item, policy))
        })
        .map(|(index, _)| index)
        .collect();
    candidates.sort_by_key(|index| {
        std::cmp::Reverse(density(&media[*index]).unwrap_or_default())
    });
    candidates
}

/// Inspect a Plex decision response without assuming metadata, media, parts,
/// or stream type fields are present.
pub fn decision_is_transcoding(
    decision: &MediaContainerWrapper<MediaContainer>,
) -> bool {
    decision
        .media_container
        .metadata
        .first()
        .and_then(|metadata| metadata.media.first())
        .and_then(|media| media.parts.first())
        .is_some_and(|part| {
            part.streams.iter().any(|stream| {
                stream.stream_type == Some(1)
                    && stream.decision.as_deref() == Some("transcode")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(
        id: i64,
        resolution: Option<&str>,
        width: Option<i64>,
        height: Option<i64>,
    ) -> Media {
        Media {
            id,
            video_resolution: resolution.map(str::to_string),
            width,
            height,
            ..Default::default()
        }
    }

    #[test]
    fn best_fit_prefers_highest_close_version_when_all_are_below_screen() {
        let versions = vec![
            media(1, Some("480"), Some(720), Some(480)),
            media(2, Some("720"), Some(1280), Some(720)),
            media(3, Some("1080"), Some(1920), Some(1080)),
        ];
        assert_eq!(best_fit_index(&versions, (3840, 2160)), Some(2));
    }

    #[test]
    fn fallback_indexes_survive_filtering_and_sorting() {
        let versions = vec![
            media(10, Some("4k"), Some(3840), Some(2160)),
            media(20, Some("720"), Some(1280), Some(720)),
            media(30, Some("1080"), Some(1920), Some(1080)),
        ];
        assert_eq!(fallback_indexes(&versions, 0, "4k", None), vec![2, 1]);
    }

    #[test]
    fn malformed_decision_is_not_a_transcode_or_a_panic() {
        assert!(!decision_is_transcoding(&Default::default()));
        assert_eq!(requested_media_index(Some("nonsense")), None);
    }
}
