use super::*;

#[ntex::test]
async fn authorize_redirects_to_op_with_browser_pkce() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=CH_browser&code_challenge_method=S256&state=ST_x&nonce=NO_y&scope=openid+profile+read%3Abilling")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status().as_u16(), 302, "authorize must 302 to OP");
    let loc = header_str(&resp, "location").expect("Location header");
    let redirect = url::Url::parse(&loc).expect("valid authorize URL");
    assert_eq!(
        redirect.origin().ascii_serialization(),
        "http://127.0.0.1:1"
    );
    assert_eq!(redirect.path(), "/oauth2/authorize");
    let params: Vec<_> = redirect.query_pairs().into_owned().collect();
    let values: std::collections::HashMap<_, _> = params.iter().cloned().collect();
    assert_eq!(
        values.len(),
        params.len(),
        "redirect parameters must not repeat"
    );
    for (key, expected) in [
        ("client_id", client_id()),
        ("response_type", "code"),
        ("code_challenge", "CH_browser"),
        ("code_challenge_method", "S256"),
        ("state", "ST_x"),
        ("nonce", "NO_y"),
        ("scope", "openid profile read:billing offline_access"),
        (
            "redirect_uri",
            "https://myapp.zeroship.ai/__zeroship/auth/popup-callback",
        ),
    ] {
        assert_eq!(values.get(key).map(String::as_str), Some(expected), "{key}");
    }
    assert!(!values.contains_key("prompt"));
    assert_eq!(
        header_str(&resp, "cache-control").as_deref(),
        Some("no-store")
    );
}

#[ntex::test]
async fn authorize_passes_prompt_through() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n&scope=openid&prompt=consent")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    let loc = header_str(&resp, "location").expect("Location");
    let redirect = url::Url::parse(&loc).unwrap();
    assert_eq!(
        redirect
            .query_pairs()
            .find(|(key, _)| key == "prompt")
            .map(|(_, value)| value.into_owned())
            .as_deref(),
        Some("consent")
    );
}

#[ntex::test]
async fn authorize_503_when_client_not_provisioned() {
    let (state, _files) = state();
    let mut routes = build_route_map_for(APP_ID, APP_NAME, APP_HOST, client_id());
    routes
        .get_mut(&AppId::parse(APP_ID).unwrap())
        .unwrap()
        .oauth_client_id = None;
    state.routes.update_snapshot(
        zeroship_core::types::GatewaySnapshot {
            routes,
            principal_lifecycle: Vec::new(),
            family_revocations: Vec::new(),
        },
        &crate::enforce::RateLimitRegistry::new(1000, 2000),
        &crate::enforce::ConcurrencyRegistry::new(100),
    );
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 503, "un-provisioned app must 503");
    assert_eq!(read_json(resp).await["error"], "client_not_provisioned");
}

#[ntex::test]
async fn authorize_400_when_missing_pkce_or_state_or_nonce() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;

    for (uri, why) in [
        ("/__zeroship/auth/authorize?state=s&nonce=n", "no code_challenge"),
        ("/__zeroship/auth/authorize?code_challenge=c&nonce=n", "no state"),
        ("/__zeroship/auth/authorize?code_challenge=c&state=s", "no nonce"),
        (
            "/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n&code_challenge_method=plain",
            "plain method rejected",
        ),
    ] {
        let req = test::TestRequest::get()
            .uri(uri)
            .header(http::header::HOST, APP_HOST)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 400, "{why}: {uri}");
    }
}

#[ntex::test]
async fn authorize_rejects_foreign_redirect_uri() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;
    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n&redirect_uri=https%3A%2F%2Fevil.example%2Fcb")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400, "foreign redirect_uri must 400");
}
