//! Every security header `headers::apply` sets, asserted on a REAL response.
//!
//! WHY THIS EXISTS. `apply` writes nine headers onto every auth response, and
//! its doc comment reasons carefully about all of them. Before this file, three
//! were checked against a live response - `referrer-policy`, `x-frame-options`
//! and `content-security-policy`, each in `threat_model.rs` and each as a
//! side-condition of some other test. The other six - HSTS, `nosniff`,
//! `permissions-policy`, COOP, CORP and the `no-store` default - were asserted
//! nowhere: deleting any one of them from `apply` broke no test.
//!
//! `framing_config_test.rs` is not that check either. It reads the Caddy config
//! FILES under `deploy/ops/`, so it proves the proxy is not configured to inject
//! a conflicting header. It never sends a request.
//!
//! WHAT THIS DOES NOT COVER:
//!
//! - Whether the FRAMED arm's ancestor list is the right one. This asserts that
//!   `/login` drops `X-Frame-Options` and does not say `frame-ancestors 'none'`;
//!   which specific origins it admits is `framing_config_test.rs` plus the
//!   `headers` unit tests.
//! - Whether a browser HONOURS any of these. That is the `tests/e2e_auth_ui/`
//!   tier, which is where CSP enforcement was actually observed blocking
//!   something.
//! - Routes that set their own CSP (the nonce-bearing interstitials). This
//!   asserts the baseline on a plain UI route.

use crate::common;
use common::Fixture;

/// `(header, expected exact value)` for the headers whose value is fixed.
///
/// Exact-match on purpose. A substring check would let
/// `max-age=0` pass an HSTS assertion, which is the one mistake that would
/// matter here.
const EXACT: [(&str, &str); 6] = [
    (
        "strict-transport-security",
        "max-age=63072000; includeSubDomains; preload",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), payment=(), \
         publickey-credentials-get=(self), interest-cohort=()",
    ),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
];

/// CSP directives the baseline must carry. These are the ones whose ABSENCE is
/// a security regression rather than a style choice.
const CSP_DIRECTIVES: [&str; 6] = [
    "default-src 'self'",
    "script-src 'self'",
    "style-src 'self'",
    "form-action 'self'",
    "base-uri 'none'",
    "object-src 'none'",
];

#[ntex::test]
async fn every_security_header_is_present_on_a_ui_route() {
    let fx = Fixture::boot("secheaders").await;

    let url = format!("{}/login", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &url)
        .expect("build GET")
        .send()
        .await
        .expect("send GET");

    let header = |name: &str| -> String {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };

    for (name, expected) in EXACT {
        assert_eq!(header(name), expected, "{name} on GET /login");
    }

    // `no-store` is applied as a DEFAULT, not an override, so that the two
    // public metadata documents can stay cacheable. A UI route must still get
    // it: a cached login page is a login page served to the next user of a
    // shared machine.
    assert_eq!(header("cache-control"), "no-store", "cache-control on /login");

    // The fixture configures a console origin, so /login is a FRAMED route: it
    // must carry a relaxed `frame-ancestors` and must NOT carry
    // `X-Frame-Options` at all. The two must not disagree - CSP Level 2 says a
    // UA supporting `frame-ancestors` ignores XFO, so a legacy UA honouring an
    // XFO: DENY would refuse the very iframe the CSP admits, and the immersive
    // console login would break on exactly the older browsers nobody tests.
    //
    // This is the direction that surprised the author: an empty
    // `x-frame-options` here is CORRECT, not a missing header.
    let csp = header("content-security-policy");
    assert_eq!(
        header("x-frame-options"),
        "",
        "a framed route must not send X-Frame-Options; CSP was {csp:?}"
    );
    assert!(
        csp.contains("frame-ancestors 'self'"),
        "framed /login must admit ancestors; got {csp:?}"
    );
    assert!(
        !csp.contains("frame-ancestors 'none'"),
        "framed /login must not also say 'none'; got {csp:?}"
    );

    // The fail-closed default, on a route that is NOT in the framed set. This
    // is the partner that makes the block above mean something: without it,
    // every assertion there is equally consistent with the headers simply being
    // absent everywhere.
    let forgot = fx
        .http
        .request(http::Method::GET, format!("{}/forgot", fx.auth_base))
        .expect("build GET /forgot")
        .send()
        .await
        .expect("send GET /forgot");
    let forgot_header = |name: &str| -> String {
        forgot
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        forgot_header("x-frame-options"),
        "DENY",
        "an unframed route must fail closed"
    );
    assert!(
        forgot_header("content-security-policy").contains("frame-ancestors 'none'"),
        "unframed default must be frame-ancestors 'none'; got {:?}",
        forgot_header("content-security-policy")
    );

    for directive in CSP_DIRECTIVES {
        assert!(csp.contains(directive), "CSP missing {directive:?}: {csp:?}");
    }
    // The directive that the browser tier caught being violated. Neither
    // relaxation may appear: `unsafe-hashes` is the subtler one, because it is
    // what an inline `style=` attribute would need and so is the tempting way
    // to "fix" a violation by widening the policy instead of the markup.
    assert!(
        !csp.contains("unsafe-inline") && !csp.contains("unsafe-hashes"),
        "CSP must not relax inline styles or scripts: {csp:?}"
    );

    fx.cleanup().await;
}
