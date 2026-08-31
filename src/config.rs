use crate::models::deserialize_comma_seperated_string;
use crate::resolution_policy::{
    deserialize_user_resolution_policies, PolicyEntry, ResolutionLimit,
};
use figment::{providers::Env, Figment};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
// use serde::Deserialize;

fn default_as_false() -> bool {
    false
}

/// Administrator configured identity for a Plex token that cannot be
/// resolved through Plex's own identity endpoints.
///
/// The compact form is simply a username:
/// `{"<sha256>": "jodie"}`.
///
/// The detailed form can additionally bind the token to one client
/// identifier. The token fingerprint remains the credential; the client
/// identifier is only an extra constraint.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum TokenIdentityBinding {
    Username(String),
    Detailed {
        username: String,
        #[serde(default)]
        client_identifier: Option<String>,
    },
}

impl TokenIdentityBinding {
    pub fn username(&self) -> &str {
        match self {
            Self::Username(username) => username,
            Self::Detailed { username, .. } => username,
        }
    }

    pub fn client_identifier(&self) -> Option<&str> {
        match self {
            Self::Username(_) => None,
            Self::Detailed {
                client_identifier, ..
            } => client_identifier.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
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
    /// Extra Plex tokens whose hubs/library the background warmer should
    /// pre-fetch. Each token is warmed into its own user-scoped cache scope,
    /// so accounts other than the configured admin also get cold-start-free
    /// loads. When empty, only `token` (the admin) is warmed. Comma separated.
    #[serde(default, deserialize_with = "deserialize_comma_seperated_string")]
    pub warm_tokens: Option<Vec<String>>,
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
    /// Additional Plex base URLs that may be selected through an encoded
    /// `<base32>.replex.stream` host. The primary and redirect hosts are
    /// always allowed. Comma separated.
    #[serde(default, deserialize_with = "deserialize_comma_seperated_string")]
    pub allowed_upstream_hosts: Option<Vec<String>>,
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
    pub allow_username_fallback: bool,
    /// SHA256(token) -> identity bindings for PMS/device scoped tokens that
    /// Plex cannot identify itself. Raw tokens are never stored in this map.
    /// Values may be either a username string or an object containing
    /// `username` and an optional `client_identifier` additional constraint.
    #[serde(default, deserialize_with = "deserialize_token_identity_map")]
    pub token_identity_map: HashMap<String, TokenIdentityBinding>,
    /// Deterministic clientIdentifier -> username bindings for clients whose
    /// tokens are opaque to plex.tv (PMS-scoped session tokens on some TV
    /// platforms). Legacy migration fallback only: client identifiers are
    /// client supplied and therefore weaker than token fingerprint bindings.
    /// JSON object: {"client-id": "username", ...}
    #[serde(default, deserialize_with = "deserialize_client_identity_map")]
    pub client_identity_map: HashMap<String, String>,
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

    /// Whether a decoded dynamic upstream is an administrator-approved Plex
    /// origin. Exact normalized base-URL matching prevents request Host data
    /// from turning Replex into a credential-forwarding proxy.
    pub fn allows_upstream_host(&self, candidate: &str) -> bool {
        upstream_matches_allowlist(
            candidate,
            self.host
                .iter()
                .chain(self.redirect_streams_host.iter())
                .chain(
                    self.allowed_upstream_hosts
                        .iter()
                        .flat_map(|hosts| hosts.iter()),
                )
                .map(String::as_str),
        )
    }
}

fn upstream_matches_allowlist<'a>(
    candidate: &str,
    allowed: impl IntoIterator<Item = &'a str>,
) -> bool {
    let Some(candidate) = normalize_base_url(candidate) else {
        return false;
    };
    allowed
        .into_iter()
        .filter_map(normalize_base_url)
        .any(|allowed| allowed == candidate)
}

fn normalize_base_url(value: &str) -> Option<String> {
    let mut parsed = url::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    while parsed.path().len() > 1 && parsed.path().ends_with('/') {
        let path = parsed.path().trim_end_matches('/').to_string();
        parsed.set_path(&path);
    }
    Some(parsed.to_string().trim_end_matches('/').to_string())
}

fn deserialize_json_map<'de, D, T>(
    deserializer: D,
) -> Result<HashMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
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

impl Config {
    /// Validate configuration once at startup rather than allowing malformed
    /// values to fail much later inside a request handler.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        let host = self
            .host
            .as_deref()
            .ok_or(ConfigValidationError::MissingHost)?;
        validate_base_url("REPLEX_HOST", host)?;

        if let Some(host) = self.redirect_streams_host.as_deref() {
            validate_base_url("REPLEX_REDIRECT_STREAMS_HOST", host)?;
        }
        for host in self.allowed_upstream_hosts.as_deref().unwrap_or(&[]) {
            validate_base_url("REPLEX_ALLOWED_UPSTREAM_HOSTS", host)?;
        }
        if let Some(base) = self.identity_api_base.as_deref() {
            validate_base_url("REPLEX_IDENTITY_API_BASE", base)?;
        }
        if let Some(port) = self.port {
            if port == 0 || port > u16::MAX as u64 {
                return Err(ConfigValidationError::InvalidPort(port));
            }
        }
        if self.ssl_enable
            && self
                .ssl_domain
                .as_deref()
                .map(str::trim)
                .filter(|domain| !domain.is_empty())
                .is_none()
        {
            return Err(ConfigValidationError::MissingSslDomain);
        }

        for (client_id, username) in &self.client_identity_map {
            if client_id.trim().is_empty() || username.trim().is_empty() {
                return Err(
                    ConfigValidationError::InvalidClientIdentityBinding,
                );
            }
        }
        for policy in &self.user_resolution_policies {
            if policy
                .username
                .as_deref()
                .is_some_and(|username| username.trim().is_empty())
                || policy
                    .uuid
                    .as_deref()
                    .is_some_and(|uuid| uuid.trim().is_empty())
            {
                return Err(ConfigValidationError::InvalidPolicyIdentity);
            }
            if policy.max_bitrate.is_some_and(|bitrate| bitrate <= 0) {
                return Err(ConfigValidationError::InvalidBitrate(
                    policy.max_bitrate.unwrap_or_default(),
                ));
            }
        }

        if let Ok(raw) = std::env::var("REPLEX_DISK_CACHE_MAX_GB") {
            let gb = raw.trim().parse::<u64>().map_err(|_| {
                ConfigValidationError::InvalidDiskCacheSize(raw.clone())
            })?;
            if gb == 0 || gb.checked_mul(1024 * 1024 * 1024).is_none() {
                return Err(ConfigValidationError::InvalidDiskCacheSize(raw));
            }
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigValidationError {
    #[error("REPLEX_HOST is required")]
    MissingHost,
    #[error("{field} must be an absolute http or https base URL: {value}")]
    InvalidBaseUrl { field: &'static str, value: String },
    #[error("REPLEX_PORT must be between 1 and 65535, got {0}")]
    InvalidPort(u64),
    #[error("REPLEX_SSL_DOMAIN is required when REPLEX_SSL_ENABLE is true")]
    MissingSslDomain,
    #[error(
        "REPLEX_CLIENT_IDENTITY_MAP contains an empty client id or username"
    )]
    InvalidClientIdentityBinding,
    #[error("resolution policy identities must not be empty strings")]
    InvalidPolicyIdentity,
    #[error(
        "resolution policy max_bitrate must be greater than zero, got {0}"
    )]
    InvalidBitrate(i64),
    #[error("REPLEX_DISK_CACHE_MAX_GB must be a positive integer that fits in bytes: {0}")]
    InvalidDiskCacheSize(String),
}

fn validate_base_url(
    field: &'static str,
    value: &str,
) -> Result<(), ConfigValidationError> {
    let parsed = url::Url::parse(value).map_err(|_| {
        ConfigValidationError::InvalidBaseUrl {
            field,
            value: value.to_string(),
        }
    })?;
    let valid_scheme = matches!(parsed.scheme(), "http" | "https");
    let valid_host = parsed.host_str().is_some();
    let clean_base = parsed.query().is_none() && parsed.fragment().is_none();
    if !valid_scheme || !valid_host || !clean_base {
        return Err(ConfigValidationError::InvalidBaseUrl {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn deserialize_client_identity_map<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_json_map(deserializer)
}

fn deserialize_token_identity_map<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, TokenIdentityBinding>, D::Error>
where
    D: Deserializer<'de>,
{
    let bindings: HashMap<String, TokenIdentityBinding> =
        deserialize_json_map(deserializer)?;
    let mut validated = HashMap::with_capacity(bindings.len());

    for (fingerprint, binding) in bindings {
        let fingerprint = fingerprint.trim().to_ascii_lowercase();
        if fingerprint.len() != 64
            || !fingerprint.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(serde::de::Error::custom(format!(
                "invalid token identity fingerprint '{fingerprint}': expected 64 hexadecimal SHA256 characters"
            )));
        }

        if binding.username().trim().is_empty() {
            return Err(serde::de::Error::custom(format!(
                "token identity binding for '{fingerprint}' has an empty username"
            )));
        }
        if matches!(
            binding.client_identifier(),
            Some(client_identifier) if client_identifier.trim().is_empty()
        ) {
            return Err(serde::de::Error::custom(format!(
                "token identity binding for '{fingerprint}' has an empty client_identifier"
            )));
        }

        validated.insert(fingerprint, binding);
    }

    Ok(validated)
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

impl Config {
    // Note the `nested` option on both `file` providers. This makes each
    // top-level dictionary act as a profile.
    pub fn figment() -> Figment {
        Figment::new().merge(Env::prefixed("REPLEX_"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_upstream_requires_an_exact_normalized_allowlist_match() {
        let allowed = ["http://plex.local:32400/", "https://remote.example"];
        assert!(upstream_matches_allowlist(
            "http://plex.local:32400",
            allowed
        ));
        assert!(upstream_matches_allowlist(
            "https://remote.example/",
            allowed
        ));
        assert!(!upstream_matches_allowlist(
            "http://attacker.invalid",
            allowed
        ));
        assert!(!upstream_matches_allowlist(
            "http://plex.local:32400@attacker.invalid",
            allowed
        ));
    }
}
