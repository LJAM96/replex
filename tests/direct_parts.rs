use httpmock::prelude::*;
use salvo::http::StatusCode;
use salvo::test::TestClient;
use salvo::Service;

/// Direct /library/parts protection scenarios, run sequentially because they
/// share env vars and the global part-policy cache.
#[tokio::test]
async fn direct_part_protection_scenarios() {
    let mock = MockServer::start();

    std::env::set_var("REPLEX_HOST", mock.base_url());
    std::env::set_var("REPLEX_IDENTITY_API_BASE", mock.base_url());
    std::env::set_var("REPLEX_RESOLUTION_POLICY_ENABLED", "true");
    std::env::set_var("REPLEX_REDIRECT_STREAMS", "true");
    std::env::set_var("REPLEX_STRICT_STREAM_GUARD", "false");
    std::env::set_var(
        "REPLEX_USER_RESOLUTION_POLICIES",
        r#"[{"username": "jodiemy3", "max_resolution": "1080"}]"#,
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
                    {"id": 1, "videoResolution": "1080", "width": 1920, "height": 1080,
                     "Part": [{"id": 111, "key": "/library/parts/1/111/file.mkv"}]},
                    {"id": 2, "videoResolution": "4k", "width": 3840, "height": 2160,
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

    // Decision flow populates the part policy cache.
    let _decision = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("mediaIndex", "0");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let service = Service::new(replex::routes::route());

    async fn get_status(
        service: &Service,
        path: &str,
        token: &str,
    ) -> StatusCode {
        let url = format!("http://127.0.0.1:5800{}", path);
        let mut res = TestClient::get(&url)
            .add_header("Host", "127.0.0.1:5800", true)
            .add_header("X-Plex-Token", token, true)
            .add_header("X-Plex-Client-Identifier", "test-client", true)
            .add_header("Accept", "application/json", true)
            .send(service)
            .await;
        res.status_code.unwrap_or(StatusCode::OK)
    }

    // Populate the cache through a playback decision as the restricted user.
    let decision_url = "http://127.0.0.1:5800/video/:/transcode/universal/decision?path=%2Flibrary%2Fmetadata%2F100&mediaIndex=1";
    let mut res = TestClient::get(decision_url)
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

    // --- restricted user: the 1080p part redirects normally ---
    let status =
        get_status(&service, "/library/parts/1/111/file.mkv", "jodie-token")
            .await;
    assert_eq!(
        status,
        StatusCode::TEMPORARY_REDIRECT,
        "permitted part must still redirect"
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

    // --- unknown part with strict stream guard disabled: allowed ---
    let status =
        get_status(&service, "/library/parts/9/999/file.mkv", "jodie-token")
            .await;
    assert_eq!(
        status,
        StatusCode::TEMPORARY_REDIRECT,
        "unknown parts fall back to legacy behaviour without strict guard"
    );

    // --- unknown part with strict stream guard enabled: rejected ---
    std::env::set_var("REPLEX_STRICT_STREAM_GUARD", "true");
    let status =
        get_status(&service, "/library/parts/9/999/file.mkv", "jodie-token")
            .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "strict guard rejects parts with no known policy"
    );

    // Known-good part still works under strict guard.
    let status =
        get_status(&service, "/library/parts/1/111/file.mkv", "jodie-token")
            .await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);

    // --- transcode session route: authenticated restricted user redirects ---
    let status = get_status(
        &service,
        "/video/:/transcode/universal/session/abc123/base-index.m3u8",
        "jodie-token",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TEMPORARY_REDIRECT,
        "authenticated session requests redirect after identity check"
    );
}
