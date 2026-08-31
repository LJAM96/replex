use httpmock::prelude::*;
use replex::config::Config;
use replex::models::PlexContext;
use replex::plex_client::{IdentityError, PlexClient};
use sha2::{Digest, Sha256};

fn token_fingerprint(token: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(token.as_bytes()))
}

fn client_for(token: Option<&str>) -> PlexClient {
    let context = PlexContext {
        token: token.map(str::to_string),
        client_identifier: Some("replex-test".to_string()),
        ..PlexContext::default()
    };
    let mut construction_context = context.clone();
    if construction_context.token.is_none() {
        construction_context.token =
            Some("replex-test-missing-token".to_string());
    }
    let mut client = PlexClient::from_context(&construction_context).unwrap();
    client.context = context;
    client.host = "http://localhost:32400".to_string();
    client
}

fn client_for_host(token: Option<&str>, host: &str) -> PlexClient {
    let mut c = client_for(token);
    c.host = host.to_string();
    c
}

// The identity API base is process-wide config, so all scenarios run
// sequentially inside one test to avoid racing the env var and to keep
// cache state predictable.
#[tokio::test]
async fn identity_resolution_scenarios() {
    // IDENTITY_CACHE and other globals extract the full Config on first use,
    // which requires these to be present.
    std::env::set_var("REPLEX_HOST", "http://localhost:32400");
    for var in [
        "REPLEX_TOKEN",
        "REPLEX_TOKEN_IDENTITY_MAP",
        "REPLEX_CLIENT_IDENTITY_MAP",
        "REPLEX_ALLOW_USERNAME_FALLBACK",
    ] {
        std::env::remove_var(var);
    }

    let mock = MockServer::start();
    std::env::set_var("REPLEX_IDENTITY_API_BASE", mock.base_url());
    let root_mock = mock.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{"machineIdentifier":"test-machine"}}"#);
    });

    // --- valid token resolves identity ---
    let user_mock = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "good-token");
        then.status(200)
            .header("content-type", "application/json")
            .body_from_file("tests/mock/in/identity_user.json");
    });

    let client = client_for(Some("good-token"));
    let identity = client.get_current_user().await.unwrap();

    assert_eq!(identity.id, 839319108);
    assert_eq!(identity.uuid, "test-uuid-1234");
    assert_eq!(identity.username, "jodiemy3");
    assert_eq!(user_mock.hits(), 1);

    // --- second lookup with same token comes from cache ---
    let cached = client.get_current_user().await.unwrap();
    assert_eq!(cached, identity);
    assert_eq!(
        user_mock.hits(),
        1,
        "second lookup must be served from cache"
    );

    // --- invalid token rejected ---
    let bad_mock = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "bad-token");
        then.status(401);
    });

    let client = client_for_host(Some("bad-token"), &mock.base_url());
    match client.get_current_user().await {
        Err(IdentityError::InvalidToken) => {}
        other => {
            panic!("expected InvalidToken, got {:?}", other.map(|i| i.username))
        }
    }
    assert_eq!(bad_mock.hits(), 1);

    // --- missing token never calls upstream ---
    // Scoped to a token no other scenario uses so it cannot swallow
    // unrelated requests (httpmock matches mocks in creation order).
    let unused_mock = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "token-no-test-uses");
        then.status(200).body("{}");
    });

    let client = client_for(None);
    match client.get_current_user().await {
        Err(IdentityError::MissingToken) => {}
        other => {
            panic!("expected MissingToken, got {:?}", other.map(|i| i.username))
        }
    }
    assert_eq!(unused_mock.hits(), 0);

    // --- upstream outage surfaces as Upstream error ---
    let err_mock = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "outage-token");
        then.status(500);
    });

    let client = client_for(Some("outage-token"));
    match client.get_current_user().await {
        Err(IdentityError::Upstream(_)) => {}
        other => panic!(
            "expected Upstream error, got {:?}",
            other.map(|i| i.username)
        ),
    }
    assert_eq!(err_mock.hits(), 1);

    // --- username falls back to title when missing ---
    let fallback_mock = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "title-only-token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{ "id": 42, "uuid": "uuid-42", "title": "Title Only" }"#);
    });

    let client = client_for(Some("title-only-token"));
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "Title Only");
    assert_eq!(fallback_mock.hits(), 1);

    // --- cached identity survives an upstream outage ---
    // jodie-token is cached from earlier; make plex.tv fail for everyone.
    let mut outage = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "good-token");
        then.status(503);
    });

    let client = client_for(Some("good-token"));
    let recovered = client.get_current_user().await.unwrap();
    assert_eq!(
        recovered.username, "jodiemy3",
        "cached identity must survive upstream outage"
    );

    // A brand new token cannot resolve while plex.tv is failing.
    let client = client_for(Some("never-seen-token"));
    match client.get_current_user().await {
        Err(IdentityError::Upstream(_)) => {}
        other => panic!(
            "expected Upstream error during outage, got {:?}",
            other.map(|i| i.username)
        ),
    }
    outage.delete();

    // --- shared (server-scoped) token resolves via resources endpoint ---
    // plex.tv /user rejects it; /resources reveals sourceTitle for our
    // machineIdentifier.
    let _shared_user = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "shared-token");
        then.status(401)
            .header("content-type", "application/json")
            .body(r#"{"errors":[{"code":1001,"message":"User could not be authenticated","status":401}]}"#);
    });

    let shared_resources = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/resources")
            .header("X-Plex-Token", "shared-token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"[{"clientIdentifier":"test-machine","provides":"server","sourceTitle":"jodiemy3","ownerId":839319108,"accessToken":"scoped"}]"#);
    });

    let root_hits_before_shared_lookup = root_mock.hits();
    let client = client_for_host(Some("shared-token"), &mock.base_url());
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "jodiemy3");
    assert_eq!(identity.uuid, "shared-jodiemy3");
    assert_eq!(root_mock.hits(), root_hits_before_shared_lookup + 1);
    assert_eq!(shared_resources.hits(), 1);

    // --- device-scoped shared tokens resolve via admin shared_servers ---
    // Some shared tokens are rejected by every plex.tv /api/v2 endpoint
    // even though the media server accepts them. The admin-authed
    // shared_servers listing maps accessTokens to usernames.
    let admin_token = "admin-token-value";
    std::env::set_var("REPLEX_TOKEN", admin_token);

    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "device-scoped-token");
        then.status(401);
    });
    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/resources")
            .header("X-Plex-Token", "device-scoped-token");
        then.status(401);
    });
    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/servers/test-machine/shared_servers")
            .header("X-Plex-Token", admin_token);
        then.status(200)
            .header("content-type", "application/xml")
            .body(
                r#"<MediaContainer size="2">
<SharedServer id="1" username="jodiemy3" userID="839319108" accessToken="other-user-token"/>
<SharedServer id="2" username="Luke.Mulvaney" userID="567660830" accessToken="device-scoped-token"/>
</MediaContainer>"#,
            );
    });

    let client = client_for_host(Some("device-scoped-token"), &mock.base_url());
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "Luke.Mulvaney");
    assert_eq!(identity.uuid, "shared-Luke.Mulvaney");

    // Second lookup must be served from the identity cache.
    let cached = client.get_current_user().await.unwrap();
    assert_eq!(cached, identity);

    // --- opaque token resolves through an administrator configured SHA256
    // fingerprint binding after every Plex-backed identity method fails ---
    let opaque_token = "opaque-fingerprint-token";
    let opaque_fingerprint = token_fingerprint(opaque_token);
    let verified_override_token = "verified-overrides-fingerprint";
    let verified_override_fingerprint =
        token_fingerprint(verified_override_token);
    std::env::set_var(
        "REPLEX_TOKEN_IDENTITY_MAP",
        format!(
            r#"{{"{opaque_fingerprint}":"fingerprint-user","{verified_override_fingerprint}":"must-not-win"}}"#
        ),
    );

    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", opaque_token);
        then.status(401);
    });

    let client = client_for_host(Some(opaque_token), &mock.base_url());
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "fingerprint-user");
    assert_eq!(identity.uuid, format!("token-sha256-{opaque_fingerprint}"));

    // The client identifier is not the credential. Another opaque token
    // presenting the exact same client identifier cannot inherit the first
    // token's administrator binding.
    let unrelated_token = "opaque-same-client-different-token";
    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", unrelated_token);
        then.status(401);
    });
    let client = client_for_host(Some(unrelated_token), &mock.base_url());
    match client.get_current_user().await {
        Err(IdentityError::InvalidToken) => {}
        other => panic!(
            "same client identifier must not inherit another token binding, got {:?}",
            other.map(|i| i.username)
        ),
    }

    // A real Plex identity always wins over an administrator fallback entry
    // for the same token fingerprint.
    let verified_override = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", verified_override_token);
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"id":77,"uuid":"verified-uuid","username":"verified-user"}"#);
    });
    let client =
        client_for_host(Some(verified_override_token), &mock.base_url());
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "verified-user");
    assert_eq!(identity.uuid, "verified-uuid");
    assert_eq!(verified_override.hits(), 1);

    // Detailed token bindings may add a client identifier constraint. A
    // mismatch is terminal and cannot fall through to weaker identity modes.
    let constrained_token = "opaque-constrained-token";
    let constrained_fingerprint = token_fingerprint(constrained_token);
    std::env::set_var(
        "REPLEX_TOKEN_IDENTITY_MAP",
        format!(
            r#"{{"{opaque_fingerprint}":"fingerprint-user","{constrained_fingerprint}":{{"username":"constrained-user","client_identifier":"bound-client"}}}}"#
        ),
    );
    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", constrained_token);
        then.status(401);
    });

    let client = client_for_host(Some(constrained_token), &mock.base_url());
    match client.get_current_user().await {
        Err(IdentityError::InvalidToken) => {}
        other => panic!(
            "client constraint mismatch must reject the binding, got {:?}",
            other.map(|i| i.username)
        ),
    }

    let mut client = client_for_host(Some(constrained_token), &mock.base_url());
    client.context.client_identifier = Some("bound-client".to_string());
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "constrained-user");

    // Username-header identity remains disabled unless explicitly enabled.
    // This proves a spoofed username cannot become an identity by default.
    std::env::remove_var("REPLEX_TOKEN_IDENTITY_MAP");
    let username_disabled_token = "username-fallback-disabled";
    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", username_disabled_token);
        then.status(401);
    });
    let mut client =
        client_for_host(Some(username_disabled_token), &mock.base_url());
    client.context.username = Some("spoofed-user".to_string());
    match client.get_current_user().await {
        Err(IdentityError::InvalidToken) => {}
        other => panic!(
            "username fallback must be disabled by default, got {:?}",
            other.map(|i| i.username)
        ),
    }

    let username_enabled_token = "username-fallback-enabled";
    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", username_enabled_token);
        then.status(401);
    });
    std::env::set_var("REPLEX_ALLOW_USERNAME_FALLBACK", "true");
    let mut client =
        client_for_host(Some(username_enabled_token), &mock.base_url());
    client.context.username = Some("explicit-username-user".to_string());
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "explicit-username-user");
    std::env::remove_var("REPLEX_ALLOW_USERNAME_FALLBACK");

    // The old clientIdentifier mapping remains available for migration, but
    // only when an administrator explicitly configures the legacy map.
    let legacy_token = "legacy-client-map-token";
    mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", legacy_token);
        then.status(401);
    });
    std::env::set_var(
        "REPLEX_CLIENT_IDENTITY_MAP",
        r#"{"replex-test":"legacy-user"}"#,
    );
    let client = client_for_host(Some(legacy_token), &mock.base_url());
    let identity = client.get_current_user().await.unwrap();
    assert_eq!(identity.username, "legacy-user");

    // Fingerprints are configuration credentials and must be unambiguous.
    // Reject malformed keys rather than silently accepting a mapping that can
    // never match a real SHA256 token fingerprint.
    std::env::set_var(
        "REPLEX_TOKEN_IDENTITY_MAP",
        r#"{"not-a-sha256":"invalid-user"}"#,
    );
    assert!(
        Config::figment().extract::<Config>().is_err(),
        "malformed token identity fingerprints must fail configuration parsing"
    );

    for var in [
        "REPLEX_TOKEN",
        "REPLEX_TOKEN_IDENTITY_MAP",
        "REPLEX_CLIENT_IDENTITY_MAP",
        "REPLEX_ALLOW_USERNAME_FALLBACK",
    ] {
        std::env::remove_var(var);
    }
}
