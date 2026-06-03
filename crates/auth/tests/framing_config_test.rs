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

/// Extract the `host:` value nested under `serve: -> admin:` in a Hydra YAML.
///
/// Minimal indentation-aware walk (no YAML dep in dev-deps): find the top-level
/// `serve:` block, then the `admin:` child within it, then that child's `host:`
/// scalar. Strips any trailing `# …` comment. Returns `None` if the path is
/// absent. This deliberately reads the *admin* host specifically — the public
/// API (`serve.public.host`) is expected to bind broadly; the privileged admin
/// API (port 4445) is the one that must stay loopback in the prod template.
fn hydra_admin_host(yaml: &str) -> Option<String> {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let lines: Vec<&str> = yaml.lines().collect();

    // Locate top-level `serve:` (indent 0).
    let serve_at = lines
        .iter()
        .position(|l| l.trim_start() == "serve:" && indent(l) == 0)?;
    let serve_indent = indent(lines[serve_at]);

    // Within the serve block, find the `admin:` child key.
    let mut admin_at = None;
    for (i, l) in lines.iter().enumerate().skip(serve_at + 1) {
        if l.trim().is_empty() {
            continue;
        }
        if indent(l) <= serve_indent {
            break; // left the serve block
        }
        if l.trim_start() == "admin:" {
            admin_at = Some(i);
            break;
        }
    }
    let admin_at = admin_at?;
    let admin_indent = indent(lines[admin_at]);

    // Within the admin block, find the `host:` scalar.
    for l in lines.iter().skip(admin_at + 1) {
        if l.trim().is_empty() {
            continue;
        }
        if indent(l) <= admin_indent {
            break; // left the admin block
        }
        let t = l.trim_start();
        if let Some(rest) = t.strip_prefix("host:") {
            let val = rest.split('#').next().unwrap_or("").trim();
            return Some(val.to_string());
        }
    }
    None
}

/// The privileged Hydra admin API (port 4445) is unauthenticated. The PROD
/// template (`ops/hydra.yaml`) must bind it to loopback (`127.0.0.1`) so a
/// verbatim copy to production never exposes `accept_login`/`accept_consent`/
/// client-registration/introspection to the network. The dev compose template
/// (`ops/hydra-dev.yaml`) legitimately binds `0.0.0.0` to share the compose
/// network and is intentionally NOT asserted here. (Review finding I1.)
#[test]
fn prod_hydra_admin_api_binds_loopback() {
    let yaml = read_ops("hydra.yaml");
    let host = hydra_admin_host(&yaml)
        .expect("ops/hydra.yaml must declare serve.admin.host");
    assert_eq!(
        host, "127.0.0.1",
        "ops/hydra.yaml serve.admin.host must be 127.0.0.1 (loopback), not \
         `{host}` — the unauthenticated admin API (:4445) must never bind a \
         routable interface in the prod template (review finding I1)"
    );
}

/// Extract the top-level `dsn:` scalar from a Hydra YAML, stripping any
/// trailing `# …` comment. Returns `None` if absent (Hydra then reads the DSN
/// from the `DSN` env var, which is the secure fail-closed default).
fn hydra_dsn(yaml: &str) -> Option<String> {
    for l in yaml.lines() {
        // Top-level key only (no leading indentation).
        if l.len() != l.trim_start().len() {
            continue;
        }
        if let Some(rest) = l.trim_start().strip_prefix("dsn:") {
            let val = rest.split('#').next().unwrap_or("").trim();
            return Some(val.to_string());
        }
    }
    None
}

/// The PROD Hydra template (`ops/hydra.yaml`) must NOT embed a postgres
/// *superuser* DSN literal. Hydra owns only its `oauth_hydra` schema (changeset
/// 0027 provisions a dedicated least-privileged login role + schema); connecting
/// as the `postgres` superuser defeats that isolation (BYPASSRLS over every
/// tenant table, tables scattered into the `zeroship` schema) and commits a
/// plaintext DB password to source. The prod template must therefore reference
/// the least-priv `oauth_hydra` role — and prefer leaving `dsn:` out entirely so
/// Hydra fails closed onto the injected `DSN` secret env var. (Review finding I2.)
#[test]
fn prod_hydra_dsn_is_not_a_superuser_literal() {
    let yaml = read_ops("hydra.yaml");
    match hydra_dsn(&yaml) {
        // Preferred: no inline DSN — Hydra reads it from the injected `DSN`
        // secret env var (fail-closed). Trivially passes the role assertion.
        None => {}
        Some(dsn) => {
            // Reject the postgres-superuser role in the DSN userinfo.
            let userinfo = dsn
                .split("://")
                .nth(1)
                .and_then(|rest| rest.split('@').next())
                .unwrap_or("");
            let user = userinfo.split(':').next().unwrap_or("");
            assert_ne!(
                user, "postgres",
                "ops/hydra.yaml dsn must NOT use the postgres SUPERUSER role \
                 (`{dsn}`) — use the least-priv `oauth_hydra` role (changeset \
                 0027) or omit `dsn:` and inject the `DSN` secret env var \
                 (review finding I2)"
            );
            assert_eq!(
                user, "oauth_hydra",
                "ops/hydra.yaml dsn, if present, must connect as the least-priv \
                 `oauth_hydra` role (changeset 0027), got `{user}` in `{dsn}` \
                 (review finding I2)"
            );
        }
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
