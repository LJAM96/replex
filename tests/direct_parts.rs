use httpmock::prelude::*;
use moka::future::Cache;
use replex::config::Config;
use replex::plex_client::PartMediaClassification;
use replex::state::AppState;
use salvo::http::StatusCode;
use salvo::test::TestClient;
use salvo::Service;
use std::sync::Arc;

fn service_with_part_cache(
    part_media_cache: Cache<i64, PartMediaClassification>,
) -> Service {
    let config: Config = Config::figment()
        .extract()
        .expect("test configuration should be valid");
    let mut state =
        AppState::new(config).expect("test application state should be valid");
    state.part_media_cache = part_media_cache;
    Service::new(replex::routes::route_with_state(Arc::new(state)))
}

/// Direct /library/parts protection scenarios, run sequentially because they
/// share env vars and the global part-classification cache.
#[tokio::test]
async fn direct_part_protection_scenarios() {
    let mock = MockServer::start();

    for var in [
        "REPLEX_TOKEN",
        "REPLEX_TOKEN_IDENTITY_MAP",
        "REPLEX_CLIENT_IDENTITY_MAP",
        "REPLEX_ALLOW_USERNAME_FALLBACK",
    ] {
        std::env::remove_var(var);
    }
    std::env::set_var("REPLEX_HOST", mock.base_url());
    std::env::set_var("REPLEX_IDENTITY_API_BASE", mock.base_url());
    std::env::set_var("REPLEX_RESOLUTION_POLICY_ENABLED", "true");
    std::env::set_var("REPLEX_REDIRECT_STREAMS", "true");
    std::env::set_var(
        "REPLEX_USER_RESOLUTION_POLICIES",
        r#"[{"username": "jodiemy3", "max_resolution": "1080"},
            {"username": "capped-only", "max_resolution": "unlimited", "max_bitrate": 8000}]"#,
    );
    std::env::set_var("REPLEX_RESOLUTION_DEFAULT", "unlimited");

    let _jodie = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "jodie-token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"id": 839319108, "uuid": "uuid-jodie", "username": "jodiemy3"}"#);
    });
    let _admin = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "admin-token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"id": 1, "uuid": "uuid-admin", "username": "admin"}"#);
    });
    let _expired = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "expired-token");
        then.status(401);
    });
    let _capped = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "capped-token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"id": 3, "uuid": "uuid-capped", "username": "capped-only"}"#);
    });

    // Item with parts: 1080p media (id 1) contains part 111,
    // 4K media (id 2) contains part 222.
    let item_json = r#"{
        "MediaContainer": {
            "Metadata": [{
                "ratingKey": "100",
                "key": "/library/metadata/100",
                "title": "Dual Version Movie",
                "type": "movie",
                "Media": [
                    {"id": 1, "videoResolution": "1080", "width": 1920, "height": 1080, "bitrate": 5000,
                     "Part": [{"id": 111, "key": "/library/parts/1/111/file.mkv"}]},
                    {"id": 2, "videoResolution": "4k", "width": 3840, "height": 2160, "bitrate": 20000,
                     "Part": [{"id": 222, "key": "/library/parts/2/222/file.mkv"}]}
                ]
            }]
        }
    }"#;
    let _item = mock.mock(|when, then| {
        when.method(GET).path("/library/metadata/100");
        then.status(200)
            .header("content-type", "application/json")
            .body(item_json);
    });

    // Decision flow populates immutable part classification facts.
    let _decision = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("mediaIndex", "0");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    // Stream paths used when delivery is proxied through Replex.
    let mut stream_mocks = Vec::new();
    for path in [
        "/library/parts/1/111/file.mkv",
        "/library/parts/2/222/file.mkv",
        "/library/parts/9/999/file.mkv",
        "/video/:/transcode/universal/session/abc123/base-index.m3u8",
    ] {
        stream_mocks.push(mock.mock(move |when, then| {
            when.method(GET).path(path);
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body("proxied-bytes");
        }));
    }

    let initial_config: Config = Config::figment()
        .extract()
        .expect("test configuration should be valid");
    let initial_state = AppState::new(initial_config)
        .expect("test application state should be valid");
    let part_media_cache = initial_state.part_media_cache.clone();
    let mut service =
        Service::new(replex::routes::route_with_state(Arc::new(initial_state)));

    async fn get_status(
        service: &Service,
        path: &str,
        token: &str,
    ) -> StatusCode {
        get_status_with_product(service, path, token, None).await
    }

    async fn get_status_with_product(
        service: &Service,
        path: &str,
        token: &str,
        product: Option<&str>,
    ) -> StatusCode {
        let url = format!("http://127.0.0.1:5800{}", path);
        let mut client = TestClient::get(&url)
            .add_header("Host", "127.0.0.1:5800", true)
            .add_header("X-Plex-Token", token, true)
            .add_header("X-Plex-Client-Identifier", "test-client", true)
            .add_header("Accept", "application/json", true);
        if let Some(product) = product {
            client = client.add_header("X-Plex-Product", product, true);
        }
        let res = client.send(service).await;
        res.status_code.unwrap_or(StatusCode::OK)
    }

    // Populate the cache through a playback decision as the restricted user.
    let decision_url = "http://127.0.0.1:5800/video/:/transcode/universal/decision?path=%2Flibrary%2Fmetadata%2F100&mediaIndex=1";
    let res = TestClient::get(decision_url)
        .add_header("Host", "127.0.0.1:5800", true)
        .add_header("X-Plex-Token", "jodie-token", true)
        .add_header("X-Plex-Client-Identifier", "test-client", true)
        .add_header("Accept", "application/json", true)
        .send(&service)
        .await;
    assert_eq!(res.status_code.unwrap_or(StatusCode::OK), StatusCode::OK);

    // --- restricted user: direct access to the 4K part is rejected ---
    let status =
        get_status(&service, "/library/parts/2/222/file.mkv", "jodie-token")
            .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "4K part must be blocked");

    // The client product header is untrusted compatibility input. Plexamp may
    // skip optional transforms, but it must not bypass the mandatory direct
    // part guard for a restricted account.
    let status = get_status_with_product(
        &service,
        "/library/parts/2/222/file.mkv",
        "jodie-token",
        Some("Plexamp"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "Plexamp product header must not bypass direct part enforcement"
    );

    let status = get_status_with_product(
        &service,
        "/library/parts/2/222/file.mkv?path=%2Flivetv%2Fsession%2F123",
        "jodie-token",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "Live TV path classification must not bypass direct part enforcement"
    );

    // --- restricted user: permitted bytes always stay behind Replex ---
    let status =
        get_status(&service, "/library/parts/1/111/file.mkv", "jodie-token")
            .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "restricted permitted part must proxy even when redirects are enabled"
    );

    // --- unrestricted user: even the 4K part redirects ---
    let status =
        get_status(&service, "/library/parts/2/222/file.mkv", "admin-token")
            .await;
    assert_eq!(
        status,
        StatusCode::TEMPORARY_REDIRECT,
        "unrestricted users keep full access"
    );

    let status =
        get_status(&service, "/library/parts/1/111/file.mkv", "capped-token")
            .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bitrate-capped account must proxy even with unlimited resolution"
    );

    let status =
        get_status(&service, "/library/parts/2/222/file.mkv", "capped-token")
            .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bitrate-capped account must not receive an original part above its cap"
    );

    // --- unrestricted account obeys redirect=false and proxies ---
    std::env::set_var("REPLEX_REDIRECT_STREAMS", "false");
    service = service_with_part_cache(part_media_cache.clone());
    let status =
        get_status(&service, "/library/parts/2/222/file.mkv", "admin-token")
            .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unrestricted account must proxy when redirects are disabled"
    );
    std::env::set_var("REPLEX_REDIRECT_STREAMS", "true");
    service = service_with_part_cache(part_media_cache.clone());

    // --- unknown parts are always rejected for restricted accounts ---
    let status =
        get_status(&service, "/library/parts/9/999/file.mkv", "jodie-token")
            .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "restricted accounts must never stream an unclassified part"
    );

    // A policy change is evaluated against the existing cached media fact;
    // no account-specific permission cache needs to be refreshed.
    std::env::set_var(
        "REPLEX_USER_RESOLUTION_POLICIES",
        r#"[{"username": "jodiemy3", "max_resolution": "4k"}]"#,
    );
    service = service_with_part_cache(part_media_cache.clone());
    let status =
        get_status(&service, "/library/parts/2/222/file.mkv", "jodie-token")
            .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "current policy must be re-evaluated against cached part classification"
    );
    std::env::set_var(
        "REPLEX_USER_RESOLUTION_POLICIES",
        r#"[{"username": "jodiemy3", "max_resolution": "1080"},
            {"username": "capped-only", "max_resolution": "unlimited", "max_bitrate": 8000}]"#,
    );
    service = service_with_part_cache(part_media_cache.clone());

    // --- transcode session route: restricted user is proxied ---
    let status = get_status(
        &service,
        "/video/:/transcode/universal/session/abc123/base-index.m3u8",
        "jodie-token",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "restricted transcode session must stay behind Replex"
    );

    // Fail-open removes the restriction decision but must not alter the
    // configured transport mode.
    std::env::set_var("REPLEX_RESOLUTION_POLICY_FAIL_CLOSED", "false");
    std::env::set_var("REPLEX_REDIRECT_STREAMS", "true");
    service = service_with_part_cache(part_media_cache.clone());
    let status =
        get_status(&service, "/library/parts/9/999/file.mkv", "expired-token")
            .await;
    assert_eq!(
        status,
        StatusCode::TEMPORARY_REDIRECT,
        "fail-open with redirects enabled must redirect"
    );

    std::env::set_var("REPLEX_REDIRECT_STREAMS", "false");
    service = service_with_part_cache(part_media_cache);
    let status =
        get_status(&service, "/library/parts/9/999/file.mkv", "expired-token")
            .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "fail-open with redirects disabled must proxy"
    );
}
