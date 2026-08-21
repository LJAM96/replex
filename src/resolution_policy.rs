use crate::models::Media;
use serde::{Deserialize, Deserializer};

/// Maximum permitted media resolution for an account.
///
/// Ordering is by strictness: `P480` is the most restrictive, `Unlimited`
/// imposes no limit. The derived `PartialOrd` treats earlier variants as
/// "less than" later ones, so `media_limit <= item_limit` means allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum ResolutionLimit {
    P480,
    P720,
    P1080,
    P2160,
    #[default]
    Unlimited,
}

impl ResolutionLimit {
    /// Parse a configured limit string. Accepts the forms documented in
    /// README/config: 480, 720, 1080, 4k, unlimited (case insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "480" => Some(ResolutionLimit::P480),
            "720" => Some(ResolutionLimit::P720),
            "1080" | "2k" => Some(ResolutionLimit::P1080),
            "4k" | "2160" => Some(ResolutionLimit::P2160),
            "unlimited" | "" => Some(ResolutionLimit::Unlimited),
            _ => None,
        }
    }

    /// True when this limit permits any resolution (no restriction).
    pub fn is_unrestricted(&self) -> bool {
        *self == ResolutionLimit::Unlimited
    }
}

/// A single user's policy entry from configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEntry {
    pub username: Option<String>,
    pub uuid: Option<String>,
    pub max_resolution: ResolutionLimit,
    /// Optional maximum video bitrate in kbps. When set, playback requests
    /// requesting more are capped to this value.
    pub max_bitrate: Option<i64>,
    /// Collection titles this account may see despite the global hidden
    /// default. Exact match against the collection title.
    pub visible_collections: Vec<String>,
}

/// A one-entry JSON deserializer target used while parsing config.
#[derive(Debug, Deserialize)]
struct RawPolicyEntry {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    max_resolution: String,
    #[serde(default)]
    max_bitrate: Option<i64>,
    #[serde(default)]
    visible_collections: Vec<String>,
}

pub fn deserialize_user_resolution_policies<'de, D>(
    deserializer: D,
) -> Result<Vec<PolicyEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    // Environment variables arrive as plain strings (figment does not parse
    // JSON in env values), while file-based config provides real sequences.
    // Accept both.
    let value = serde_json::Value::deserialize(deserializer)?;
    let raw: Vec<RawPolicyEntry> = match value {
        serde_json::Value::String(s) => {
            serde_json::from_str(&s).map_err(serde::de::Error::custom)?
        }
        v => serde_json::from_value(v).map_err(serde::de::Error::custom)?,
    };

    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        if entry.username.is_none() && entry.uuid.is_none() {
            continue;
        }
        let max_resolution = match ResolutionLimit::parse(&entry.max_resolution) {
            Some(limit) => limit,
            None => {
                return Err(serde::de::Error::custom(format!(
                    "unknown max_resolution '{}', expected one of: 480, 720, 1080, 4k, unlimited",
                    entry.max_resolution
                )))
            }
        };
        out.push(PolicyEntry {
            username: entry.username,
            uuid: entry.uuid,
            max_resolution,
            max_bitrate: entry.max_bitrate,
            visible_collections: entry.visible_collections,
        });
    }
    Ok(out)
}

/// Verified Plex account identity.
///
/// Always resolved from the request's own token via the Plex API; never from
/// client-supplied username headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentity {
    pub id: i64,
    pub uuid: String,
    pub username: String,
}

/// The resolved policy for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionPolicy {
    pub limit: ResolutionLimit,
    /// Maximum video bitrate in kbps, when the matched policy sets one.
    pub max_bitrate: Option<i64>,
    /// Collection titles this account must not see (global hidden default
    /// minus the account's explicit exceptions).
    pub hidden_collections: Vec<String>,
}

impl ResolutionPolicy {
    pub fn unrestricted() -> Self {
        ResolutionPolicy {
            limit: ResolutionLimit::Unlimited,
            max_bitrate: None,
            hidden_collections: Vec::new(),
        }
    }

    pub fn is_unrestricted(&self) -> bool {
        self.limit.is_unrestricted()
    }
}

/// Resolve the policy for a verified identity.
///
/// Matching order: account uuid first (stable identifier), then username.
/// Users with no matching entry receive the configured default. The identity
/// must come from token verification; username headers on requests are never
/// consulted here.
///
/// `default_hidden_collections` are hidden from everyone; accounts with a
/// matching entry see them removed from their hidden list via
/// `visible_collections`.
pub fn resolve_policy(
    policies: &[PolicyEntry],
    default_limit: ResolutionLimit,
    default_hidden_collections: &[String],
    identity: &UserIdentity,
) -> ResolutionPolicy {
    let matched = policies
        .iter()
        .find(|p| p.uuid.as_deref() == Some(identity.uuid.as_str()))
        .or_else(|| {
            policies.iter().find(|p| {
                p.username.as_deref() == Some(identity.username.as_str())
            })
        });

    let limit = matched.map_or(default_limit, |p| p.max_resolution);
    let max_bitrate = matched.and_then(|p| p.max_bitrate);
    let visible = matched
        .map(|p| p.visible_collections.as_slice())
        .unwrap_or(&[]);
    let hidden_collections: Vec<String> = default_hidden_collections
        .iter()
        .filter(|title| !visible.contains(title))
        .cloned()
        .collect();

    tracing::debug!(
        username = %identity.username,
        uuid = %identity.uuid,
        ?limit,
        ?max_bitrate,
        hidden_collections = ?hidden_collections,
        "Resolution policy matched"
    );

    ResolutionPolicy {
        limit,
        max_bitrate,
        hidden_collections,
    }
}

/// Classify a media version's resolution.
///
/// Uses both the textual `videoResolution` attribute and the actual pixel
/// dimensions, taking the more restrictive interpretation of the two.
/// Returns `None` when the resolution cannot be determined; callers must not
/// treat unknown media as acceptable for restricted users.
pub fn classify(media: &Media) -> Option<ResolutionLimit> {
    let from_text = media
        .video_resolution
        .as_deref()
        .and_then(ResolutionLimit::parse);

    let from_dims = classify_dimensions(media.width, media.height);

    match (from_text, from_dims) {
        // Disagreement -> take the HIGHER classification. This is the
        // restrictive interpretation: a file labelled 1080p whose dimensions
        // are 3840x2160 is treated as 4K (blocked for 1080p users), never
        // the other way around.
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Classify purely from pixel dimensions.
///
/// Thresholds follow the design doc: a version counts as at most 1080p when
/// width <= 1920 AND height <= 1080, and at most 4K when width <= 4096 AND
/// height <= 2160. Exceeding either dimension bumps the class, so ultrawide
/// encodes such as 3840x1608 are correctly treated as 4K. Dimensions beyond
/// the 4K ceiling (e.g. 8K) are unclassifiable -> `None`.
fn classify_dimensions(
    width: Option<i64>,
    height: Option<i64>,
) -> Option<ResolutionLimit> {
    let (w, h) = match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => return None,
    };

    if w > 4096 || h > 2160 {
        return None;
    }
    if w > 1920 || h > 1080 {
        return Some(ResolutionLimit::P2160);
    }
    if w > 1280 || h > 720 {
        return Some(ResolutionLimit::P1080);
    }
    if w > 720 || h > 480 {
        return Some(ResolutionLimit::P720);
    }
    Some(ResolutionLimit::P480)
}

/// Whether a single media version is permitted under the policy.
///
/// Media whose resolution cannot be determined is NOT acceptable for
/// restricted users; it is only acceptable when the policy is unrestricted.
pub fn media_allowed(media: &Media, policy: &ResolutionPolicy) -> bool {
    if policy.is_unrestricted() {
        return true;
    }
    match classify(media) {
        Some(limit) => limit <= policy.limit,
        None => false,
    }
}

/// Filter a media list down to versions permitted by the policy.
pub fn allowed_media(media: &[Media], policy: &ResolutionPolicy) -> Vec<Media> {
    media
        .iter()
        .filter(|m| media_allowed(m, policy))
        .cloned()
        .collect()
}

/// Choose the best permitted version, preferring the highest resolution that
/// fits within both the account limit and (when known) the screen
/// resolution, mirroring the density-based preference in auto selection.
pub fn best_allowed_media(
    media: &[Media],
    policy: &ResolutionPolicy,
    screen_resolution: Option<(i64, i64)>,
) -> Option<Media> {
    let candidates = allowed_media(media, policy);
    if candidates.is_empty() {
        return None;
    }

    let device_density = screen_resolution.map(|(w, h)| w * h);

    candidates.into_iter().min_by(|a, b| {
        let key = |m: &Media| -> i64 {
            let density = m.width.unwrap_or(0) * m.height.unwrap_or(0);
            match device_density {
                Some(dd) => (density - dd).abs(),
                None => -density, // prefer highest density without a screen
            }
        };
        key(a).cmp(&key(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(
        resolution: Option<&str>,
        width: Option<i64>,
        height: Option<i64>,
    ) -> Media {
        Media {
            video_resolution: resolution.map(|s| s.to_string()),
            width,
            height,
            ..Default::default()
        }
    }

    #[test]
    fn parse_limits() {
        assert_eq!(ResolutionLimit::parse("480"), Some(ResolutionLimit::P480));
        assert_eq!(ResolutionLimit::parse("720"), Some(ResolutionLimit::P720));
        assert_eq!(
            ResolutionLimit::parse("1080"),
            Some(ResolutionLimit::P1080)
        );
        assert_eq!(ResolutionLimit::parse("4k"), Some(ResolutionLimit::P2160));
        assert_eq!(
            ResolutionLimit::parse("unlimited"),
            Some(ResolutionLimit::Unlimited)
        );
        assert_eq!(
            ResolutionLimit::parse("UNLIMITED"),
            Some(ResolutionLimit::Unlimited)
        );
        assert_eq!(ResolutionLimit::parse("bogus"), None);
    }

    #[test]
    fn ordering_is_strictness() {
        assert!(ResolutionLimit::P480 < ResolutionLimit::P1080);
        assert!(ResolutionLimit::P1080 < ResolutionLimit::P2160);
        assert!(ResolutionLimit::P2160 < ResolutionLimit::Unlimited);
    }

    // --- classification matrix from resretrict.md ---

    #[test]
    fn classify_1920x1080_allowed_for_1080() {
        let m = media(Some("1080"), Some(1920), Some(1080));
        assert_eq!(classify(&m), Some(ResolutionLimit::P1080));
        assert!(media_allowed(
            &m,
            &ResolutionPolicy {
                limit: ResolutionLimit::P1080,
                max_bitrate: None,
                hidden_collections: vec![]
            }
        ));
    }

    #[test]
    fn classify_1920x800_allowed_for_1080() {
        let m = media(None, Some(1920), Some(800));
        assert_eq!(classify(&m), Some(ResolutionLimit::P1080));
        assert!(media_allowed(
            &m,
            &ResolutionPolicy {
                limit: ResolutionLimit::P1080,
                max_bitrate: None,
                hidden_collections: vec![]
            }
        ));
    }

    #[test]
    fn classify_3840x2160_blocked_for_1080() {
        let m = media(Some("4k"), Some(3840), Some(2160));
        assert_eq!(classify(&m), Some(ResolutionLimit::P2160));
        assert!(!media_allowed(
            &m,
            &ResolutionPolicy {
                limit: ResolutionLimit::P1080,
                max_bitrate: None,
                hidden_collections: vec![]
            }
        ));
    }

    #[test]
    fn classify_3840x1608_blocked_for_1080() {
        // Ultrawide 4K: height alone would look like ~1080p-class, but the
        // width reveals it. Dimensions and text agree here.
        let m = media(Some("4k"), Some(3840), Some(1608));
        assert_eq!(classify(&m), Some(ResolutionLimit::P2160));
        assert!(!media_allowed(
            &m,
            &ResolutionPolicy {
                limit: ResolutionLimit::P1080,
                max_bitrate: None,
                hidden_collections: vec![]
            }
        ));
    }

    #[test]
    fn classify_4096x2160_allowed_for_4k() {
        let m = media(Some("4k"), Some(4096), Some(2160));
        assert_eq!(classify(&m), Some(ResolutionLimit::P2160));
        assert!(media_allowed(
            &m,
            &ResolutionPolicy {
                limit: ResolutionLimit::P2160,
                max_bitrate: None,
                hidden_collections: vec![]
            }
        ));
    }

    #[test]
    fn classify_7680x4320_blocked_for_4k() {
        // 8K exceeds the 4K ceiling.
        let m = media(None, Some(7680), Some(4320));
        assert_eq!(classify(&m), None);
        assert!(!media_allowed(
            &m,
            &ResolutionPolicy {
                limit: ResolutionLimit::P2160,
                max_bitrate: None,
                hidden_collections: vec![]
            }
        ));
    }

    #[test]
    fn text_and_dimensions_disagree_more_restrictive_wins() {
        // Labelled 1080 but dimensions are 4K -> treated as 4K (blocked for
        // 1080 users; never let a mislabel slip through).
        let m = media(Some("1080"), Some(3840), Some(2160));
        assert_eq!(classify(&m), Some(ResolutionLimit::P2160));

        // Labelled 4k but dimensions are 1080p -> still treated as 4K
        // (conservative: the label claims more than pixels show).
        let m = media(Some("4k"), Some(1920), Some(1080));
        assert_eq!(classify(&m), Some(ResolutionLimit::P2160));
    }

    #[test]
    fn unknown_resolution_blocked_for_restricted_allowed_for_unrestricted() {
        let m = media(None, None, None);
        assert_eq!(classify(&m), None);
        assert!(!media_allowed(
            &m,
            &ResolutionPolicy {
                limit: ResolutionLimit::P1080,
                max_bitrate: None,
                hidden_collections: vec![]
            }
        ));
        assert!(media_allowed(&m, &ResolutionPolicy::unrestricted()));
    }

    // --- filtering ---

    fn sample_versions() -> Vec<Media> {
        vec![
            media(Some("1080"), Some(1920), Some(1080)),
            media(Some("4k"), Some(3840), Some(2160)),
        ]
    }

    #[test]
    fn allowed_media_filters_for_1080_user() {
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::P1080,
            max_bitrate: None,
            hidden_collections: vec![],
        };
        let allowed = allowed_media(&sample_versions(), &policy);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].video_resolution.as_deref(), Some("1080"));
    }

    #[test]
    fn allowed_media_keeps_everything_for_4k_user() {
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::P2160,
            max_bitrate: None,
            hidden_collections: vec![],
        };
        assert_eq!(allowed_media(&sample_versions(), &policy).len(), 2);
    }

    #[test]
    fn allowed_media_empty_when_nothing_permitted() {
        let only_4k = vec![media(Some("4k"), Some(3840), Some(2160))];
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::P1080,
            max_bitrate: None,
            hidden_collections: vec![],
        };
        assert!(allowed_media(&only_4k, &policy).is_empty());
    }

    #[test]
    fn best_allowed_prefers_closest_to_screen_density() {
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::P2160,
            max_bitrate: None,
            hidden_collections: vec![],
        };
        let best =
            best_allowed_media(&sample_versions(), &policy, Some((1920, 1080)))
                .unwrap();
        assert_eq!(best.video_resolution.as_deref(), Some("1080"));

        let best =
            best_allowed_media(&sample_versions(), &policy, Some((3840, 2160)))
                .unwrap();
        assert_eq!(best.video_resolution.as_deref(), Some("4k"));
    }

    #[test]
    fn best_allowed_respects_account_limit_over_device() {
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::P1080,
            max_bitrate: None,
            hidden_collections: vec![],
        };
        let best = best_allowed_media(
            &sample_versions(),
            &policy,
            Some((3840, 2160)), // 4K television
        )
        .unwrap();
        assert_eq!(best.video_resolution.as_deref(), Some("1080"));
    }

    #[test]
    fn best_allowed_none_when_all_blocked() {
        let only_4k = vec![media(Some("4k"), Some(3840), Some(2160))];
        let policy = ResolutionPolicy {
            limit: ResolutionLimit::P1080,
            max_bitrate: None,
            hidden_collections: vec![],
        };
        assert!(best_allowed_media(&only_4k, &policy, None).is_none());
    }

    // --- config parsing ---

    #[test]
    fn parse_policy_entries_json() {
        let json = r#"[
            {"username": "user1080", "max_resolution": "1080"},
            {"uuid": "1234-5678", "max_resolution": "4k"},
            {"username": "admin", "max_resolution": "unlimited"}
        ]"#;
        let entries: Vec<PolicyEntry> = serde_json::from_str::<
            Vec<RawPolicyEntry>,
        >(json)
        .map_err(|_| ())
        .ok()
        .unwrap()
        .into_iter()
        .filter(|e| e.username.is_some() || e.uuid.is_some())
        .map(|e| PolicyEntry {
            username: e.username,
            uuid: e.uuid,
            max_resolution: ResolutionLimit::parse(&e.max_resolution).unwrap(),
            max_bitrate: None,
            visible_collections: vec![],
        })
        .collect();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].max_resolution, ResolutionLimit::P1080);
        assert_eq!(entries[1].max_resolution, ResolutionLimit::P2160);
        assert_eq!(entries[1].username, None);
        assert_eq!(entries[2].max_resolution, ResolutionLimit::Unlimited);
    }

    // --- policy resolution ---

    fn identity(id: i64, uuid: &str, username: &str) -> UserIdentity {
        UserIdentity {
            id,
            uuid: uuid.to_string(),
            username: username.to_string(),
        }
    }

    #[test]
    fn resolve_policy_matches_uuid_first() {
        let policies = vec![
            PolicyEntry {
                username: Some("jodie".to_string()),
                uuid: None,
                max_resolution: ResolutionLimit::P1080,
                max_bitrate: None,
                visible_collections: vec![],
            },
            PolicyEntry {
                username: Some("someone-else".to_string()),
                uuid: Some("uuid-jodie".to_string()),
                max_resolution: ResolutionLimit::P2160,
                max_bitrate: None,
                visible_collections: vec![],
            },
        ];
        let policy = resolve_policy(
            &policies,
            ResolutionLimit::Unlimited,
            &[],
            &identity(1, "uuid-jodie", "jodie"),
        );
        // uuid match wins over the username-only entry
        assert_eq!(policy.limit, ResolutionLimit::P2160);
    }

    #[test]
    fn resolve_policy_falls_back_to_username() {
        let policies = vec![PolicyEntry {
            username: Some("jodie".to_string()),
            uuid: None,
            max_resolution: ResolutionLimit::P1080,
            max_bitrate: None,
            visible_collections: vec![],
        }];
        let policy = resolve_policy(
            &policies,
            ResolutionLimit::Unlimited,
            &[],
            &identity(1, "some-uuid", "jodie"),
        );
        assert_eq!(policy.limit, ResolutionLimit::P1080);
    }

    #[test]
    fn resolve_policy_unmatched_uses_default() {
        let policies = vec![PolicyEntry {
            username: Some("jodie".to_string()),
            uuid: None,
            max_resolution: ResolutionLimit::P1080,
            max_bitrate: None,
            visible_collections: vec![],
        }];
        let policy = resolve_policy(
            &policies,
            ResolutionLimit::Unlimited,
            &[],
            &identity(1, "other-uuid", "luke"),
        );
        assert_eq!(policy.limit, ResolutionLimit::Unlimited);
    }

    #[test]
    fn resolve_policy_empty_policies_uses_default() {
        let policy = resolve_policy(
            &[],
            ResolutionLimit::P1080,
            &[],
            &identity(1, "u", "anyone"),
        );
        assert_eq!(policy.limit, ResolutionLimit::P1080);
    }

    #[test]
    fn resolve_policy_case_sensitive_usernames() {
        // Plex usernames are case sensitive; config must match exactly.
        let policies = vec![PolicyEntry {
            username: Some("Jodie".to_string()),
            uuid: None,
            max_resolution: ResolutionLimit::P1080,
            max_bitrate: None,
            visible_collections: vec![],
        }];
        let policy = resolve_policy(
            &policies,
            ResolutionLimit::Unlimited,
            &[],
            &identity(1, "u", "jodie"),
        );
        assert_eq!(policy.limit, ResolutionLimit::Unlimited);
    }

    #[test]
    fn resolve_policy_collection_visibility() {
        let jodie_entry = PolicyEntry {
            username: Some("jodiemy3".to_string()),
            uuid: None,
            max_resolution: ResolutionLimit::P1080,
            max_bitrate: None,
            visible_collections: vec!["🎀Jodie".to_string()],
        };
        let default_hidden = vec!["🎀Jodie".to_string()];

        // Jodie: exception applies, nothing hidden.
        let policy = resolve_policy(
            std::slice::from_ref(&jodie_entry),
            ResolutionLimit::Unlimited,
            &default_hidden,
            &identity(1, "uuid-j", "jodiemy3"),
        );
        assert!(policy.hidden_collections.is_empty());

        // Everyone else: hidden by default.
        let policy = resolve_policy(
            std::slice::from_ref(&jodie_entry),
            ResolutionLimit::Unlimited,
            &default_hidden,
            &identity(2, "uuid-l", "luke"),
        );
        assert_eq!(policy.hidden_collections, vec!["🎀Jodie".to_string()]);
    }
}
