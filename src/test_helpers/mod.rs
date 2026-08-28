use httpmock::prelude::*;
use std::sync::{Mutex, MutexGuard, OnceLock};

// A single shared mock server keeps REPLEX_HOST stable for every test that
// calls this, avoiding races between tests running in parallel threads that
// each set the process-wide env var to their own server.
static MOCK_SERVER: OnceLock<MockServer> = OnceLock::new();

// Serialises every test that reads or writes REPLEX_* env vars. The config
// is process-global, so concurrent tests would otherwise observe each
// other's mutations (e.g. a policy test pointing REPLEX_HOST at localhost
// while a route test is mid-request).
static ENV_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Resets the REPLEX_* vars that affect the mock-server fixtures to their
/// defaults and points REPLEX_HOST at the shared mock server. Returns the
/// env lock guard; hold it for the whole test.
pub(crate) fn pin_default_env(mock_host: &str) -> MutexGuard<'static, ()> {
    let guard = env_lock();
    std::env::set_var("REPLEX_HOST", format!("http://{mock_host}"));
    for var in [
        // Opt-in features must never leak in from other tests or the shell.
        "REPLEX_RESOLUTION_POLICY_ENABLED",
        "REPLEX_NTF_WATCHLIST_FORCE",
        "REPLEX_DISABLE_CONTINUE_WATCHING",
        "REPLEX_REDIRECT_STREAMS",
        "REPLEX_HERO_ROWS",
        "REPLEX_HIDDEN_COLLECTIONS",
        // Per-account policies and the identity endpoint are process-global;
        // leftovers from a parallel test must never narrow another test's
        // accounts.
        "REPLEX_USER_RESOLUTION_POLICIES",
        "REPLEX_IDENTITY_API_BASE",
    ] {
        std::env::remove_var(var);
    }
    guard
}

pub(crate) fn get_mock_server() -> &'static MockServer {
    MOCK_SERVER.get_or_init(|| {
        let mock_server = MockServer::start();
        let _ = mock_server.mock(|when, then| {
            when.method(GET)
                .path("/hubs/sections/6")
                .header("X-Plex-Token", "fakeID")
                .header("X-Plex-Client-Identifier", "fakeID");
            then.status(200)
                .header("content-type", "application/json")
                .body_from_file("tests/mock/in/hubs_sections_6.json");
        });

        let _ = mock_server.mock(|when, then| {
            when.method(GET)
                .path("/library/sections/6/collections")
                .header("X-Plex-Token", "fakeID")
                .header("X-Plex-Client-Identifier", "fakeID");
            then.status(200)
                .header("content-type", "application/json")
                .body_from_file(
                    "tests/mock/in/library_sections_6_collections.json",
                );
        });

        let _ = mock_server.mock(|when, then| {
            when.method(GET)
                .path("/library/collections/254688")
                .header("X-Plex-Token", "fakeID")
                .header("X-Plex-Client-Identifier", "fakeID");
            then.status(200)
                .header("content-type", "application/json")
                .body_from_file(
                    "tests/mock/in/library_collections_254688.json",
                );
        });

        let _ = mock_server.mock(|when, then| {
            when.method(GET)
                .path("/hubs/home")
                .header("X-Plex-Token", "fakeID")
                .header("X-Plex-Client-Identifier", "fakeID");
            // Recent PMS versions removed /hubs/home entirely.
            then.status(404);
        });

        // Serves the /hubs/home -> /hubs/promoted fallback fetch. Matches any
        // promoted request without contentDirectoryID params; the stricter
        // promoted mock below takes precedence for requests that have them.
        let _ = mock_server.mock(|when, then| {
            when.method(GET)
                .path("/hubs/promoted")
                .header("X-Plex-Token", "fakeID")
                .header("X-Plex-Client-Identifier", "fakeID");
            then.status(200)
                .header("content-type", "application/json")
                .body_from_file("tests/mock/in/hubs_promoted_6_7.json");
        });

        let _ = mock_server.mock(|when, then| {
            when.method(GET)
                .path("/hubs/promoted")
                .header("X-Plex-Token", "fakeID")
                .header("X-Plex-Client-Identifier", "fakeID")
                .query_param("pinnedContentDirectoryID", "6,7")
                .query_param("contentDirectoryID", "6,7");
            then.status(200)
                .header("content-type", "application/json")
                .body_from_file("tests/mock/in/hubs_promoted_6_7.json");
        });

        mock_server
    })
}
