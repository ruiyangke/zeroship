//! Phase 7 U5 — gateway `DPoP` integration surface.
//!
//! Full end-to-end coverage (real `DPoP`-bound access token from hydra
//! → gateway dispatch → worker forward) requires hydra to emit
//! `cnf.jkt` on access tokens, which it does not today. The Phase 7
//! ship target is the verification + introspection plumbing only;
//! token-binding enforcement is Phase 8+ work.
//!
//! What this file covers instead:
//!
//!   - [`introspection_response_deserialises_active_shape`] — fixture
//!     deserialisation against the RFC 7662 / hydra `/oauth2/introspect`
//!     active-token response, so a mid-flight hydra format change shows
//!     up here rather than as a silent 401.
//!   - [`introspection_response_deserialises_inactive_shape`] — the
//!     inactive (`active: false`, no other fields) shape; the gateway
//!     must treat this as "reject", not "parse error".
//!   - [`introspect_token_dead_upstream_errors`] — calling
//!     `OidcRp::introspect_token` against an unreachable URL returns
//!     `TokenExchange`, proving the call path actually dispatches over
//!     the network (the dispatch arm's `Err(_) → None` branch is the
//!     one that closes the auth gate when hydra is down).
//!
//! The real `DPoP` roundtrip — generate Ed25519 proof, sign it against a
//! resource URL, present `Authorization: DPoP <token>` to a live
//! gateway, observe `ZeroShip-User` forwarded to a worker — gets added
//! in Phase 8 alongside the binding step.

use zeroship_gateway::oidc_rp::{IntrospectionResponse, OidcRp};

#[test]
fn introspection_response_deserialises_active_shape() {
    // Verbatim shape hydra returns for an active access token (per
    // the response example in the brief). Each `Option` here gets
    // populated; the `active` discriminator is `true`.
    let body = r#"{
        "active": true,
        "client_id": "gateway",
        "sub": "usr_01H1234",
        "exp": 1700000600,
        "iat": 1700000000,
        "scope": "openid offline_access email profile",
        "email": "user@example.com",
        "name": "Test User",
        "email_verified": true
    }"#;
    let parsed: IntrospectionResponse =
        serde_json::from_str(body).expect("hydra active introspect must parse");
    assert!(parsed.active);
    assert_eq!(parsed.sub.as_deref(), Some("usr_01H1234"));
    assert_eq!(parsed.client_id.as_deref(), Some("gateway"));
    assert_eq!(parsed.email.as_deref(), Some("user@example.com"));
    assert_eq!(parsed.email_verified, Some(true));
    assert_eq!(parsed.name.as_deref(), Some("Test User"));
    assert_eq!(parsed.exp, Some(1_700_000_600));
}

#[test]
fn introspection_response_deserialises_inactive_shape() {
    // Per RFC 7662 §2.2 the AS MAY return just `{"active": false}`
    // for revoked/expired/unknown tokens. All other fields are
    // implicitly `None` and the gateway MUST reject without touching
    // them.
    let parsed: IntrospectionResponse =
        serde_json::from_str(r#"{"active": false}"#).expect("inactive shape must parse");
    assert!(!parsed.active);
    assert!(parsed.sub.is_none());
    assert!(parsed.email.is_none());
    assert!(parsed.name.is_none());
    assert!(parsed.client_id.is_none());
}

#[ntex::test]
async fn introspect_token_dead_upstream_errors() {
    // Point at a port nothing is listening on — the introspect call
    // should return TokenExchange, not panic, and not block forever.
    // 127.0.0.1:1 is the canonical "always-closed" test target.
    let rp = OidcRp::new(
        "http://127.0.0.1:1",
        "gateway",
        "test-secret",
        b"k".repeat(32),
    );
    let err = rp
        .introspect_token("any-token")
        .await
        .expect_err("dead upstream must surface as TokenExchange");
    // The error variant — `Display` includes the literal "introspect"
    // prefix from the call site, so we can tell this came from the
    // introspect path and not, say, a code-exchange call.
    let msg = err.to_string();
    assert!(
        msg.contains("introspect"),
        "error message should identify the introspect path, got: {msg}"
    );
}
