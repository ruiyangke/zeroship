//! Offline static config-assertion guard for the immersive-iframe login pivot
//! (design §3.1 / §6.3 / §9 "a static config-assertion test, offline-doable").
//!
//! The whole framed login dance runs inside ONE cross-origin iframe on the
//! console (`§3.1`). For that to work, NO response in the in-iframe navigation
//! chain may carry `X-Frame-Options` or a `frame-ancestors` that excludes the
//! console — otherwise the browser kills the navigation mid-flow and the relay
//! never fires (a silent 60-second timeout with no diagnosable error). The
//! auth-service route-aware headers ([`zeroship_auth::headers`]) handle the
//! auth `/login` / `/signup` / `/consent` documents; this test pins the two
//! INFRASTRUCTURE legs the spec calls out:
//!
//!   (a) `ops/hydra.yaml` must emit NO framing header on `/oauth2/*` (Hydra's
//!       happy-path responses are body-less 302s, but a misconfigured global
//!       framing header would still poison an HTML error/interstitial); and
//!   (b) `ops/Caddyfile` (the auth-origin reverse proxy) must inject NO global
//!       `X-Frame-Options` / `frame-ancestors` on `auth.zeroship.*`.
//!
//! These configs carry zero framing headers today; this test pins that
//! invariant against future drift (§3.1 is load-bearing).

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

/// The framing tokens that, if emitted on the auth origin / `/oauth2/*`, would
/// dead-end the framed login. Case-insensitive substring match (config keys are
/// not case-normalized, so we lowercase the haystack).
const FRAMING_TOKENS: &[&str] = &["x-frame-options", "frame-ancestors"];

#[test]
fn hydra_config_emits_no_framing_headers() {
    let yaml = read_ops("hydra.yaml").to_lowercase();
    for token in FRAMING_TOKENS {
        assert!(
            !yaml.contains(token),
            "ops/hydra.yaml must NOT emit `{token}` — a framing header on \
             /oauth2/* (or a Hydra HTML error page) would kill the in-iframe \
             login navigation (design §3.1/§6.3)"
        );
    }
}

#[test]
fn caddy_proxy_injects_no_framing_header_on_auth_origin() {
    let caddy = read_ops("Caddyfile").to_lowercase();
    for token in FRAMING_TOKENS {
        assert!(
            !caddy.contains(token),
            "ops/Caddyfile must NOT inject `{token}` on the auth origin — a \
             global framing header in front of auth.zeroship.* would block the \
             console from framing /login (design §3.1/§6.3)"
        );
    }
}
