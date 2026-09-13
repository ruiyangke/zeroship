use super::super::*;

#[test]
fn signout_body_parses_scope_json_and_form_and_defaults() {
    use ntex::web::test::TestRequest;
    let req = TestRequest::default()
        .header(http::header::CONTENT_TYPE, "application/json")
        .to_http_request();
    let b = parse_signout_body(&req, br#"{"scope":"global"}"#);
    assert_eq!(b.scope.as_deref(), Some("global"));
    let req = TestRequest::default()
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .to_http_request();
    let b = parse_signout_body(&req, b"scope=global");
    assert_eq!(b.scope.as_deref(), Some("global"));
    let req = TestRequest::default().to_http_request();
    let b = parse_signout_body(&req, b"");
    assert_eq!(b.scope, None);
}

#[test]
fn redirect_uri_override_is_exact_match_not_prefix_match() {
    let scheme = "https";
    let host = "app.zeroship.ai";

    assert!(is_registered_redirect_uri(
        scheme,
        host,
        "https://app.zeroship.ai/__zeroship/auth/popup-callback"
    ));
    assert!(is_registered_redirect_uri(
        scheme,
        host,
        "https://app.zeroship.ai/__zeroship/auth/callback"
    ));
    assert_eq!(
        default_redirect_uri(scheme, host),
        "https://app.zeroship.ai/__zeroship/auth/popup-callback"
    );

    assert!(
        !is_registered_redirect_uri(scheme, host, "https://app.zeroship.ai/evil"),
        "same-origin-but-unregistered redirect_uri must be rejected (exact-match)",
    );

    assert!(!is_registered_redirect_uri(
        scheme,
        host,
        "https://app.zeroship.ai/__zeroship/auth/popup-callback?next=//evil.com"
    ));
    assert!(!is_registered_redirect_uri(
        scheme,
        host,
        "https://app.zeroship.ai/__zeroship/auth/popup-callback/../evil"
    ));

    assert!(!is_registered_redirect_uri(
        scheme,
        host,
        "https://evil.com/__zeroship/auth/popup-callback"
    ));
    assert!(!is_registered_redirect_uri(
        scheme,
        host,
        "https://app.zeroship.ai.evil.com/__zeroship/auth/popup-callback"
    ));
}

#[test]
fn authorize_scope_includes_offline_access_once() {
    assert_eq!(scope_with_offline_access("openid"), "openid offline_access");
    assert_eq!(
        scope_with_offline_access("openid profile offline_access"),
        "openid profile offline_access"
    );
    assert_eq!(scope_with_offline_access("   "), "openid offline_access");
}

#[test]
fn authorize_query_parses_all_params() {
    let q = AuthorizeQuery::parse(
        "code_challenge=CH&code_challenge_method=S256&state=ST&nonce=NO&scope=openid+profile&prompt=consent&redirect_uri=https%3A%2F%2Fapp%2Fcb&idp_hint=google",
    );
    assert_eq!(q.code_challenge.as_deref(), Some("CH"));
    assert_eq!(q.code_challenge_method.as_deref(), Some("S256"));
    assert_eq!(q.state.as_deref(), Some("ST"));
    assert_eq!(q.nonce.as_deref(), Some("NO"));
    assert_eq!(q.scope.as_deref(), Some("openid profile"));
    assert_eq!(q.prompt.as_deref(), Some("consent"));
    assert_eq!(q.redirect_uri.as_deref(), Some("https://app/cb"));
    assert_eq!(q.idp_hint.as_deref(), Some("google"));
}
