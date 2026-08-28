use crate::models::deserialize_comma_seperated_string;
use crate::resolution_policy::{
    deserialize_user_resolution_policies, PolicyEntry, ResolutionLimit,
};
use figment::{providers::Env, Figment};
use serde::{Deserialize, Deserializer};
// use serde::Deserialize;

fn default_as_false() -> bool {
    false
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct Config {
    #[serde(deserialize_with = "deserialize_host")]
    pub host: Option<String>,
    pub token: Option<String>,
    pub port: Option<u64>,
    #[serde(
        default = "default_as_true",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub interleave: bool,
    #[serde(
        default = "default_as_true",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub hub_restrictions: bool,
    #[serde(
        default = "default_as_true",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub exclude_watched: bool,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl: u64,
    /// Hub payloads older than this are served stale while a background
    /// refresh runs. Playback changes observed through the proxy mark all
    /// hubs stale immediately. 0 disables the staleness layer.
    #[serde(default = "default_hub_stale_ttl")]
    pub hub_stale_ttl: u64,
    /// Seconds between background hub warmer cycles that pre-fetch the hot
    /// hub payloads with the admin token. 0 disables warming.
    #[serde(default = "default_warm_interval")]
    pub warm_interval: u64,
    #[serde(
        default = "default_as_true",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub cache_rows: bool,
    #[deprecated]
    #[serde(
        default = "default_as_true",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub cache_rows_refresh: bool,
    #[serde(default, deserialize_with = "deserialize_comma_seperated_string")]
    pub hero_rows: Option<Vec<String>>,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub ssl_enable: bool,
    pub ssl_domain: Option<String>,
    pub newrelic_api_key: Option<String>,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub enable_console: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub disable_continue_watching: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub disable_user_state: bool,
    #[serde(
        default = "default_as_true",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub disable_leaf_count: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub redirect_streams: bool,
    pub redirect_streams_host: Option<String>,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub disable_related: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub disable_transcode: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub force_maximum_quality: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub auto_select_version: bool,
    #[serde(default, deserialize_with = "deserialize_comma_seperated_string")]
    pub video_transcode_fallback_for: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_comma_seperated_string")]
    pub force_direct_play_for: Option<Vec<String>>,
    pub test_script: Option<String>,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub ntf_watchlist_force: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub resolution_policy_enabled: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_user_resolution_policies"
    )]
    pub user_resolution_policies: Vec<PolicyEntry>,
    #[serde(default, deserialize_with = "deserialize_resolution_default")]
    pub resolution_default: ResolutionLimit,
    #[serde(
        default = "default_as_true",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub resolution_policy_fail_closed: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub strict_stream_guard: bool,
    #[serde(
        default = "default_as_false",
        deserialize_with = "figment::util::bool_from_str_or_int"
    )]
    pub allow_username_fallback: bool,
    /// Deterministic clientIdentifier -> username bindings for clients whose
    /// tokens are opaque to plex.tv (PMS-scoped session tokens on some TV
    /// platforms). JSON object: {"client-id": "username", ...}
    #[serde(default, deserialize_with = "deserialize_client_identity_map")]
    pub client_identity_map: std::collections::HashMap<String, String>,
    #[serde(default = "default_identity_cache_ttl")]
    pub identity_cache_ttl: u64,
    pub identity_api_base: Option<String>,
    /// Collection titles hidden from accounts without an explicit exception.
    #[serde(default, deserialize_with = "deserialize_comma_seperated_string")]
    pub hidden_collections: Option<Vec<String>>,
}

fn default_identity_cache_ttl() -> u64 {
    60 * 60 // 60 minutes
}

impl Config {
    /// Base URL for the Plex account identity API. Overridable for testing.
    pub fn identity_api_base(&self) -> String {
        self.identity_api_base
            .clone()
            .unwrap_or_else(|| "https://plex.tv".to_string())
    }
}

fn deserialize_client_identity_map<'de, D>(
    deserializer: D,
) -> Result<std::collections::HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) if s.trim().is_empty() => {
            Ok(Default::default())
        }
        serde_json::Value::String(s) => {
            serde_json::from_str(&s).map_err(serde::de::Error::custom)
        }
        v => serde_json::from_value(v).map_err(serde::de::Error::custom),
    }
}

fn deserialize_resolution_default<'de, D>(
    deserializer: D,
) -> Result<ResolutionLimit, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<String> = Deserialize::deserialize(deserializer)?;
    match s {
        None => Ok(ResolutionLimit::Unlimited),
        Some(s) => ResolutionLimit::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!(
                "unknown resolution_default '{}', expected one of: 480, 720, 1080, 4k, unlimited",
                s
            ))),
    }
}

fn default_cache_ttl() -> u64 {
    30 * 60 // 30 minutes
}

fn default_hub_stale_ttl() -> u64 {
    5 * 60 // 5 minutes
}

fn default_warm_interval() -> u64 {
    5 * 60 // 5 minutes
}

pub(crate) fn deserialize_host<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Deserialize::deserialize(deserializer)? {
        Some::<String>(mut s) => {
            if s.ends_with('/') {
                s.pop();
            }
            Ok(Some(s))
        }
        None => Ok(None),
    }
}

fn default_as_true() -> bool {
    true
}

fn deserialize_hosr() -> bool {
    true
}

impl Config {
    // Note the `nested` option on both `file` providers. This makes each
    // top-level dictionary act as a profile.
    pub fn figment() -> Figment {
        Figment::new().merge(Env::prefixed("REPLEX_"))
    }

    pub fn dynamic(req: &salvo::Request) -> Figment {
        let mut config = Config::figment();
        // Every header/param is treated as potentially hostile or malformed.
        // Any failure here simply means "no replex.stream host override" — we
        // must never panic on a client-supplied value.
        let host = match req.headers().get("HOST").and_then(|v| v.to_str().ok())
        {
            Some(h) => h,
            None => return config,
        };
        if host.contains("replex.stream") {
            use data_encoding::BASE32;
            let val: Vec<&str> = host.split(".replex.stream").collect();
            let owned_val = match val.first() {
                Some(v) => v.to_ascii_uppercase(),
                None => return config,
            };
            let Ok(decoded_len) = BASE32.decode_len(owned_val.len()) else {
                return config;
            };
            let mut output = vec![0u8; decoded_len];
            let Ok(len) = BASE32.decode_mut(owned_val.as_bytes(), &mut output)
            else {
                return config;
            };
            if len == 0 {
                return config;
            }
            match std::str::from_utf8(&output[0..len]) {
                Ok(host_value) => {
                    config = config.join(("host", host_value));
                }
                Err(_) => return config,
            }
        }
        config
        // Figment::new().merge(Env::prefixed("REPLEX_"))
    }
    // pub fn default() -> Self {
    //     Config { include_watched: false}
    // }
}
