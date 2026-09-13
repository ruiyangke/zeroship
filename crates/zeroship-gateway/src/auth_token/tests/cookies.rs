use super::*;

#[ntex::test]
async fn foreign_origin_is_rejected_no_cors_reflection() {
    let op = Arc::new(MockOP::new(client_id()));
    let (base, _srv) = boot_mock_op(op).await;
    let (state, _files) = build_state(&base, None);

    let app = test::init_service(anchors_app!(state)).await;

    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/session")
        .header("host", APP_HOST)
        .header("origin", "https://evil.example.com")
        .header("x-zs-auth", "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403, "foreign Origin must be 403");
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "MUST NOT reflect a foreign Origin (no credentialed CORS oracle)"
    );

    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/session")
        .header("host", APP_HOST)
        .header("origin", "null")
        .header("x-zs-auth", "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403, "Origin: null must be 403");
}

#[ntex::test]
async fn token_missing_custom_header_is_rejected() {
    let op = Arc::new(MockOP::new(client_id()));
    let (base, _srv) = boot_mock_op(op).await;
    let (state, _files) = build_state(&base, None);
    let app = test::init_service(anchors_app!(state)).await;

    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/session")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[ntex::test]
async fn session_mint_without_custom_header_is_rejected() {
    let op = Arc::new(MockOP::new(client_id()));
    let (base, _srv) = boot_mock_op(op).await;
    let (state, _files) = build_state(&base, None);
    let app = test::init_service(anchors_app!(state)).await;

    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/session?mint=1")
        .header("host", APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[ntex::test]
async fn session_get_fast_path_honors_valid_pairwise_cookie_db_free() {
    let op = Arc::new(MockOP::new(client_id()));
    let (base, _srv) = boot_mock_op(op).await;
    let (state, _files) = build_state(&base, None);
    assert!(
        state.db.is_none(),
        "fixture must have no DB for the DB-free proof"
    );

    let pws = test_pairwise_subject(&UserId::mint(), APP_HOST);
    let scopes = vec!["openid".to_string(), "email".to_string()];
    let cookie = issue_session_cookie(&state, &pws, &scopes);

    let app = test::init_service(anchors_app!(state)).await;
    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/session")
        .header("host", APP_HOST)
        .header("cookie", cookie)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "valid pws_ cookie must be honored DB-free"
    );
    let body = read_json(resp).await;
    assert_eq!(
        body["user"]["id"], pws,
        "fast path projects the cookie's pws_ sub"
    );
    assert_eq!(
        body["user"]["email"], "relay-alias@zeroship.ai",
        "relay alias projected straight from the cookie claim"
    );
}

#[ntex::test]
async fn session_get_fast_path_rejects_non_pairwise_sub() {
    let op = Arc::new(MockOP::new(client_id()));
    let (base, _srv) = boot_mock_op(op).await;
    let (state, _files) = build_state(&base, None);

    let global_like = UserId::mint().as_str().to_owned();
    assert!(
        !zeroship_core::auth::is_pairwise_subject(&global_like),
        "a global user ID is not a pairwise subject"
    );
    let cookie = issue_session_cookie(&state, &global_like, &[]);

    let app = test::init_service(anchors_app!(state)).await;
    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/session")
        .header("host", APP_HOST)
        .header("cookie", cookie)
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status().as_u16();
    assert_eq!(
        status, 503,
        "invalid identity falls through to unavailable recovery"
    );
    let body = read_json(resp).await;
    assert_ne!(
        body["user"]["id"], global_like,
        "the non-pws_ sub must never appear in a projected user body"
    );
}

#[ntex::test]
async fn session_post_fails_fast_without_signing_key() {
    let op = Arc::new(MockOP::new(client_id()));
    let (base, _srv) = boot_mock_op(op).await;

    let (mut state, _files) = build_state(&base, None);
    {
        let st = Arc::get_mut(&mut state).expect("sole owner before init_service");
        st.signing_key = None;
        st.session_issuer = None;
        st.session_verifier = None;
    }
    assert!(state.session_issuer.is_none());
    assert!(state.db.is_none());

    let app = test::init_service(anchors_app!(state)).await;
    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/session")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("x-zs-auth", "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        503,
        "missing signing key must 503 before any OP/DB work"
    );
    let body = read_json(resp).await;
    assert_eq!(
        body["error"], "session_signing_unavailable",
        "must surface the signing-key failure, NOT db_unavailable (proves fail-fast ordering)"
    );
}

#[ntex::test]
async fn token_exchange_is_identity_only_and_sets_both_cookies() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        seed_user(&database.admin, &op.user_id).await;
        let user_id = op.user_id.clone();
        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db));
        let app = test::init_service(anchors_app!(state.clone())).await;

        let req = test::TestRequest::post()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload("grant_type=authorization_code&code=thecode&code_verifier=theverifier")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "code exchange must succeed");

        assert_eq!(
            resp.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );

        let session_cookie = set_cookie_with_prefix(&resp, "__Host-zeroship_app_session=")
            .expect("live session cookie set");
        assert!(session_cookie.contains("HttpOnly"));
        assert!(session_cookie.contains("SameSite=Lax"));
        assert!(session_cookie.contains("Secure"));
        assert!(
            session_cookie.contains("Max-Age=900"),
            "short signed-cookie TTL: {session_cookie}"
        );
        let session_token = session_cookie
            .split(';')
            .next()
            .unwrap()
            .split('=')
            .nth(1)
            .unwrap()
            .to_string();

        let anchor_cookie = set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=")
            .expect("anchor cookie set");
        assert!(anchor_cookie.contains("HttpOnly"));
        assert!(anchor_cookie.contains("SameSite=Strict"));
        assert!(anchor_cookie.contains("Secure"));
        let breadcrumb = set_cookie_with_prefix(&resp, "zs.myapp.zeroship.ai.is.authenticated=")
            .expect("breadcrumb set");
        assert!(
            !breadcrumb.contains("HttpOnly"),
            "breadcrumb must be JS-readable"
        );

        let body: serde_json::Value = read_json(resp).await;

        assert!(
            body.get("access_token").is_none(),
            "no access_token in body"
        );
        assert!(body.get("token_type").is_none(), "no token_type in body");
        assert!(body.get("expires_in").is_none(), "no expires_in in body");
        assert!(body.get("scope").is_none(), "no scope in body");
        assert!(body.get("id_token").is_none(), "no id_token in body");
        assert!(
            body["user"].get("scopes").is_none(),
            "user projection must carry NO scopes"
        );
        assert!(body["expires_at"].is_i64(), "expires_at must be present");

        let expected_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id,
            &format!("https://{APP_HOST}"),
        );
        assert!(expected_pws.starts_with("pws_"), "{expected_pws}");
        assert_eq!(body["user"]["id"], expected_pws);
        assert_ne!(body["user"]["id"], user_id.as_str());
        let raw_body = serde_json::to_string(&body).unwrap();
        assert!(
            !raw_body.contains(user_id.as_str()),
            "global UUID must NOT appear in the identity body"
        );

        let claims = state
            .session_verifier
            .as_ref()
            .expect("session verifier")
            .verify(&session_token, client_id())
            .expect("signed session cookie verifies locally");
        assert_eq!(
            claims.app,
            client_id(),
            "cookie app binds to the route client_id"
        );
        assert_eq!(claims.sub, expected_pws, "cookie sub is the per-app pws_");
        assert!(
            !serde_json::to_string(&claims)
                .unwrap()
                .contains(user_id.as_str()),
            "global UUID must NOT appear in the signed session cookie"
        );

        {
            let conn = &database.admin;
            let rows = conn
                .query(
                    "SELECT user_id, app_id FROM zeroship.gateway_sessions \
                 WHERE user_id = $1 AND app_id = $2",
                    &[&user_id.as_str(), &APP_ID],
                )
                .await
                .expect("audit row query");
            assert!(
                !rows.is_empty(),
                "POST /session must WRITE the gateway_sessions audit/revocation row"
            );
        }
    })
    .await;
}

#[ntex::test]
async fn token_exchange_swaps_email_for_relay_alias() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        seed_user(&database.admin, &op.user_id).await;
        let user_id = op.user_id.clone();
        let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
        seed_relay_alias(&database.admin, &user_id, &relay_email).await;
        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db));
        let app = test::init_service(anchors_app!(state.clone())).await;

        let req = test::TestRequest::post()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload("grant_type=authorization_code&code=thecode&code_verifier=theverifier")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "code exchange must succeed");

        let body: serde_json::Value = read_json(resp).await;

        let expected_pws = test_pairwise_subject(&user_id, APP_HOST);
        assert_eq!(
            body["user"]["id"],
            serde_json::json!(expected_pws),
            "relay projection must preserve the route's deterministic pairwise identity"
        );

        assert_eq!(body["user"]["email"], serde_json::json!(relay_email));
        assert_ne!(body["user"]["email"], serde_json::json!(REAL_EMAIL));
        let raw_body = serde_json::to_string(&body).unwrap();
        assert!(
            !raw_body.contains(REAL_EMAIL),
            "real email must be ABSENT from the identity body"
        );
    })
    .await;
}

#[ntex::test]
async fn token_exchange_fails_closed_when_no_alias() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        seed_user(&database.admin, &op.user_id).await;
        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db));
        let app = test::init_service(anchors_app!(state.clone())).await;

        let req = test::TestRequest::post()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload("grant_type=authorization_code&code=thecode&code_verifier=theverifier")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "exchange still succeeds");

        let body: serde_json::Value = read_json(resp).await;

        assert_eq!(
            body["user"]["email"],
            serde_json::json!(""),
            "no alias ⇒ empty email (fail closed)"
        );
        let raw_body = serde_json::to_string(&body).unwrap();
        assert!(
            !raw_body.contains(REAL_EMAIL),
            "real email must be ABSENT even when no alias exists (fail closed)"
        );
    })
    .await;
}

#[ntex::test]
async fn session_steady_state_projects_cookie_without_op() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        seed_user(&database.admin, &op.user_id).await;
        let user_id = op.user_id.clone();
        let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
        seed_relay_alias(&database.admin, &user_id, &relay_email).await;
        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db_cfg = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db_cfg.clone()));
        let app = test::init_service(anchors_app!(state.clone())).await;

        let req = test::TestRequest::post()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200);
        let session_cookie =
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_session=").expect("session cookie");
        let session_pair = session_cookie.split(';').next().unwrap().to_string();

        let before = op.refresh_calls.load(Ordering::SeqCst);
        let req = test::TestRequest::get()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("cookie", session_pair)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = read_json(resp).await;

        let expected_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id,
            &format!("https://{APP_HOST}"),
        );
        assert_eq!(body["user"]["id"], expected_pws);
        assert_eq!(body["user"]["email"], serde_json::json!(relay_email));
        assert!(
            body.get("access_token").is_none(),
            "no JWT in steady-state body"
        );
        assert!(body["expires_at"].is_i64());
        let raw_body = serde_json::to_string(&body).unwrap();
        assert!(!raw_body.contains(REAL_EMAIL), "real email absent");

        assert_eq!(
            op.refresh_calls.load(Ordering::SeqCst),
            before,
            "a live-session read must skip OP entirely (no family rotation)"
        );
    })
    .await;
}

#[ntex::test]
async fn session_minted_cookie_verifies_locally_bound_to_route_client() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        seed_user(&database.admin, &op.user_id).await;
        let user_id = op.user_id.clone();
        let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
        seed_relay_alias(&database.admin, &user_id, &relay_email).await;
        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db_cfg = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db_cfg.clone()));
        let app = test::init_service(anchors_app!(state.clone())).await;

        let req = test::TestRequest::post()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200);
        let session_cookie =
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_session=").expect("session cookie");
        let session_token = session_cookie
            .strip_prefix("__Host-zeroship_app_session=")
            .and_then(|rest| rest.split(';').next())
            .expect("session token in cookie")
            .to_string();

        let verifier = state.session_verifier.as_ref().expect("session verifier");
        let claims = verifier
            .verify(&session_token, client_id())
            .expect("the POST /session-minted cookie MUST verify under the route client_id");
        let expected_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id,
            &format!("https://{APP_HOST}"),
        );
        assert_eq!(
            claims.app,
            client_id(),
            "cookie binds to the route client_id (app claim)"
        );
        assert_eq!(claims.sub, expected_pws, "cookie sub is the per-app pws_");
        assert_eq!(
            claims.email, relay_email,
            "cookie carries the relay alias, never the real email"
        );
        assert!(
            !serde_json::to_string(&claims)
                .unwrap()
                .contains(user_id.as_str()),
            "global UUID must not appear in the signed cookie"
        );

        assert!(
            verifier.verify(&session_token, bcl_client_id()).is_err(),
            "a cookie minted for client_id() MUST NOT verify for a different app client_id"
        );
    })
    .await;
}
