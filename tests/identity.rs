use httpmock::prelude::*;
use replex::models::{PlexContext};
use replex::plex_client::{IdentityError, PlexClient};

fn client_for(token: Option<&str>) -> PlexClient {
    let mut context = PlexContext::default();
    context.token = token.map(|t| t.to_string());
    context.client_identifier = Some("replex-test".to_string());

    PlexClient {
        http_client: reqwest_middleware::ClientBuilder::new(
            reqwest::Client::new(),
        )
        .build(),
        context,
        host: "http://localhost:32400".to_string(),
        cache: moka::future::Cache::builder().max_capacity(10).build(),
        default_headers: http::HeaderMap::new(),
    }
}

// The identity API base is process-wide config, so all scenarios run
// sequentially inside one test to avoid racing the env var and to keep
// cache state predictable.
#[tokio::test]
async fn identity_resolution_scenarios() {
    // IDENTITY_CACHE and other globals extract the full Config on first use,
    // which requires these to be present.
    std::env::set_var("REPLEX_HOST", "http://localhost:32400");

    let mock = MockServer::start();
    std::env::set_var("REPLEX_IDENTITY_API_BASE", mock.base_url());

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

    let client = client_for(Some("bad-token"));
    match client.get_current_user().await {
        Err(IdentityError::InvalidToken) => {}
        other => panic!(
            "expected InvalidToken, got {:?}",
            other.map(|i| i.username)
        ),
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
        other => panic!(
            "expected MissingToken, got {:?}",
            other.map(|i| i.username)
        ),
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
}
