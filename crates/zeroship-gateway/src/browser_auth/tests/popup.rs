use super::*;

#[ntex::test]
async fn popup_callback_pins_its_nonce_and_keeps_query_values_out_of_html() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;

    let xss = "<script>alert(1)</script>";
    let uri = format!(
        "/__zeroship/auth/popup-callback?code=abc&state=st&error_description={}",
        url::form_urlencoded::byte_serialize(xss.as_bytes()).collect::<String>()
    );
    let req = test::TestRequest::get()
        .uri(&uri)
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);

    let csp = header_str(&resp, "content-security-policy").expect("CSP");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("script-src 'nonce-"), "{csp}");
    assert!(csp.contains("frame-ancestors 'self'"), "{csp}");
    assert!(!csp.contains("unsafe-inline"), "no unsafe-inline: {csp}");
    assert_eq!(
        header_str(&resp, "referrer-policy").as_deref(),
        Some("no-referrer")
    );
    assert_eq!(
        header_str(&resp, "cross-origin-opener-policy").as_deref(),
        Some("same-origin")
    );

    let nonce = csp
        .split("script-src 'nonce-")
        .nth(1)
        .and_then(|s| s.split('\'').next())
        .expect("nonce in CSP")
        .to_string();

    let body = read_text(resp).await;
    assert!(
        body.contains(&format!("<script nonce=\"{nonce}\">")),
        "script nonce must match CSP nonce: body={body}"
    );
    assert!(
        !body.contains(xss),
        "raw query must NOT be reflected: {body}"
    );
    assert!(!body.contains("alert(1)"), "no reflected script: {body}");
    assert!(!body.contains("code=abc"), "no reflected code: {body}");
}

#[ntex::test]
async fn popup_callback_nonce_is_per_response() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;

    let nonce_of = |csp: String| {
        csp.split("script-src 'nonce-")
            .nth(1)
            .and_then(|s| s.split('\'').next())
            .map(str::to_string)
    };
    let r1 = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/__zeroship/auth/popup-callback")
            .header(http::header::HOST, APP_HOST)
            .to_request(),
    )
    .await;
    let r2 = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/__zeroship/auth/popup-callback")
            .header(http::header::HOST, APP_HOST)
            .to_request(),
    )
    .await;
    let n1 = nonce_of(header_str(&r1, "content-security-policy").unwrap());
    let n2 = nonce_of(header_str(&r2, "content-security-policy").unwrap());
    assert!(n1.is_some() && n2.is_some());
    assert_ne!(n1, n2, "CSP nonce must be fresh per response");
}
