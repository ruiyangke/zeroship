//! Offline static config-assertion guard for the immersive-iframe login pivot
//! (design §3.1 / §6.3 / §9 "a static config-assertion test, offline-doable").
//!
//! The whole framed login dance runs inside ONE cross-origin iframe on the
//! console (`§3.1`). For that to work, framed auth documents must be exactly the
//! native interstitial routes, and the auth-origin reverse proxy must not inject
//! global framing headers that override the route-aware service headers.

use std::path::PathBuf;

/// Workspace root = the crate manifest dir (`crates/auth`) climbed two parents.
fn ops_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("ops")
        .join(file)
}

fn read_ops(file: &str) -> String {
    let path = ops_path(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn native_interstitial_routes_are_frameable() {
    for path in ["/login", "/signup", "/consent"] {
        assert!(
            zeroship_auth::headers::is_framed_route_for_test(path),
            "{path} must remain frameable for the console login iframe"
        );
    }
}

#[test]
fn non_interstitial_auth_routes_are_not_frameable() {
    for path in ["/oauth2/authorize", "/oauth/google/start", "/healthz"] {
        assert!(
            !zeroship_auth::headers::is_framed_route_for_test(path),
            "{path} must not inherit the framed-route header relax"
        );
    }
}

#[test]
fn caddy_proxy_injects_no_x_frame_options_on_auth_origin() {
    let caddy = read_ops("Caddyfile").to_lowercase();
    assert!(
        !caddy.contains("x-frame-options"),
        "ops/Caddyfile must NOT inject `x-frame-options` on the auth origin"
    );
}

#[test]
fn caddy_proxy_injects_no_frame_ancestors_on_auth_origin() {
    let caddy = read_ops("Caddyfile").to_lowercase();
    assert!(
        !caddy.contains("frame-ancestors"),
        "ops/Caddyfile must NOT inject `frame-ancestors` on the auth origin"
    );
}
