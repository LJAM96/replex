use httpmock::prelude::*;
use salvo::http::StatusCode;
use salvo::test::TestClient;
use salvo::Service;

// All scenarios share process-wide env vars and global caches, so they run
// sequentially inside one test with distinct tokens per scenario.
#[tokio::test]
async fn playback_enforcement_scenarios() {
    let mock = MockServer::start();

    std::env::set_var("REPLEX_HOST", mock.base_url());
    std::env::set_var("REPLEX_IDENTITY_API_BASE", mock.base_url());
    std::env::set_var("REPLEX_RESOLUTION_POLICY_ENABLED", "true");
    std::env::set_var(
        "REPLEX_USER_RESOLUTION_POLICIES",
        r#"[{"username": "jodiemy3", "max_resolution": "1080", "max_bitrate": 4000},
            {"username": "sd-only", "max_resolution": "480"},
            {"username": "capped-only", "max_resolution": "unlimited", "max_bitrate": 8000}]"#,
    );
    std::env::set_var("REPLEX_RESOLUTION_DEFAULT", "unlimited");

    // Identities
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
    let _sd = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "sd-token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"id": 9, "uuid": "uuid-sd", "username": "sd-only"}"#);
    });
    let _capped_only = mock.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/user")
            .header("X-Plex-Token", "capped-token");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"id": 11, "uuid": "uuid-capped", "username": "capped-only"}"#);
    });

    // Item: index 0 = 1080p (id 1), index 1 = 4K (id 2)
    let item_json = r#"{
        "MediaContainer": {
            "Metadata": [{
                "ratingKey": "100",
                "key": "/library/metadata/100",
                "title": "Dual Version Movie",
                "type": "movie",
                "Media": [
                    {"id": 1, "videoResolution": "1080", "width": 1920, "height": 1080},
                    {"id": 2, "videoResolution": "4k", "width": 3840, "height": 2160}
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

    let service = Service::new(replex::routes::route());

    async fn send_decision(
        service: &Service,
        token: &str,
        extra_query: &str,
        username_header: Option<&str>,
    ) -> StatusCode {
        let url = format!(
            "http://127.0.0.1:5800/video/:/transcode/universal/decision?path=%2Flibrary%2Fmetadata%2F100&{}",
            extra_query
        );
        let mut client = TestClient::get(&url)
            .add_header("Host", "127.0.0.1:5800", true)
            .add_header("X-Plex-Token", token, true)
            .add_header("X-Plex-Client-Identifier", "test-client", true)
            .add_header("Accept", "application/json", true);
        if let Some(username) = username_header {
            client = client.add_header("X-Plex-Username", username, true);
        }
        let mut res = client.send(service).await;
        res.status_code.unwrap_or(StatusCode::OK)
    }

    // --- restricted user: prohibited mediaIndex rewritten to allowed ---
    let rewritten = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("mediaIndex", "0")
            .query_param("scn", "1");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status =
        send_decision(&service, "jodie-token", "mediaIndex=1&scn=1", None)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        rewritten.hits() >= 1,
        "upstream must receive mediaIndex rewritten to the 1080p version"
    );

    // --- restricted user: allowed mediaIndex passes through ---
    let passthrough = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("mediaIndex", "0")
            .query_param("scn", "2");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status =
        send_decision(&service, "jodie-token", "mediaIndex=0&scn=2", None)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(passthrough.hits() >= 1);

    // --- restricted user: no mediaIndex gets pinned to allowed version ---
    let pinned = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("mediaIndex", "0")
            .query_param("scn", "3");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status = send_decision(&service, "jodie-token", "scn=3", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        pinned.hits() >= 1,
        "mediaIndex must be pinned before proxying"
    );

    // --- unrestricted user: untouched ---
    let untouched = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("mediaIndex", "1")
            .query_param("scn", "4");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status =
        send_decision(&service, "admin-token", "mediaIndex=1&scn=4", None)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        untouched.hits() >= 1,
        "unrestricted users must reach upstream unchanged"
    );

    // --- invalid token fails closed ---
    let unreachable = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("scn", "5");
        then.status(200).body(r#"{"MediaContainer":{}}"#);
    });

    let status =
        send_decision(&service, "expired-token", "mediaIndex=1&scn=5", None)
            .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "fail-closed must reject playback when identity cannot be verified"
    );
    assert_eq!(unreachable.hits(), 0, "nothing may reach upstream");

    // --- spoofed username does not change policy ---
    let spoofed = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("mediaIndex", "0")
            .query_param("scn", "6");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status = send_decision(
        &service,
        "jodie-token",
        "mediaIndex=1&scn=6",
        Some("admin"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        spoofed.hits() >= 1,
        "spoofed username must not bypass the rewrite"
    );

    // --- no permitted version returns 403 ---
    let status =
        send_decision(&service, "sd-token", "mediaIndex=0", None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "no permitted version must yield 403"
    );

    // --- bitrate cap: request above cap is lowered ---
    // jodiemy3 has max_bitrate 4000; she asks for 20000.
    let capped = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("maxVideoBitrate", "4000");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status =
        send_decision(&service, "jodie-token", "mediaIndex=0&scn=7&maxVideoBitrate=20000", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(capped.hits() >= 1, "bitrate must be capped to the policy max");

    // --- bitrate cap: request below cap is left alone ---
    let below = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("maxVideoBitrate", "2000");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status =
        send_decision(&service, "jodie-token", "mediaIndex=0&scn=8&maxVideoBitrate=2000", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(below.hits() >= 1, "lower requests must not be raised");

    // --- bitrate-only policy: unlimited resolution user still capped ---
    let capped_only = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("maxVideoBitrate", "8000")
            .query_param("mediaIndex", "1")
            .query_param("scn", "9");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    // capped-only is resolution-unlimited: mediaIndex=1 (4K) passes through,
    // but the bitrate is capped.
    let status =
        send_decision(&service, "capped-token", "mediaIndex=1&scn=9&maxVideoBitrate=50000", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        capped_only.hits() >= 1,
        "resolution-unlimited users with a bitrate cap must still be capped"
    );

    // --- no bitrate requested: cap injected ---
    let injected = mock.mock(|when, then| {
        when.method(GET)
            .path("/video/:/transcode/universal/decision")
            .query_param("maxVideoBitrate", "8000")
            .query_param("mediaIndex", "1")
            .query_param("scn", "10");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"MediaContainer":{}}"#);
    });

    let status = send_decision(&service, "capped-token", "mediaIndex=1&scn=10", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(injected.hits() >= 1, "cap must be injected when absent");
}
