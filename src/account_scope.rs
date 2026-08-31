use sha2::{Digest, Sha256};

/// Full irreversible fingerprint used where a stable credential identity is
/// required (for example administrator-provided token identity bindings).
pub fn token_fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    data_encoding::HEXLOWER.encode(&digest)
}

/// Compact credential scope for logs and account-isolated cache keys.
pub fn token_scope(token: Option<&str>) -> String {
    token
        .map(|token| token_fingerprint(token)[..16].to_string())
        .unwrap_or_else(|| "anon".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_stable_and_do_not_contain_credentials() {
        let token = "secret-plex-token";
        let scope = token_scope(Some(token));
        assert_eq!(scope.len(), 16);
        assert_eq!(scope, token_scope(Some(token)));
        assert!(!scope.contains(token));
        assert_eq!(token_scope(None), "anon");
        assert_eq!(token_fingerprint(token).len(), 64);
    }
}
