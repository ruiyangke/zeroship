use super::*;

#[ntex::test]
async fn malformed_refresh_response_does_not_expose_tokens_in_errors() {
    let op = Arc::new(MockOP::new(client_id()));
    op.garbage_2xx.store(true, Ordering::SeqCst);
    let (base, _srv) = boot_mock_op(op.clone()).await;
    let oidc = OidcRp::new(
        &base,
        BrokerSecret::from_bytes(TEST_BROKER_MASTER.to_vec()).expect("broker secret"),
        b"k".repeat(32),
    )
    .with_issuer(MOCK_ISSUER);

    let err = oidc
        .refresh_token_public(client_id(), "rt_seed")
        .await
        .expect_err("garbage 2xx body must fail to parse");
    let msg = err.to_string();
    assert!(msg.contains("parse:"), "expected a parse error, got: {msg}");
    assert!(
        !msg.contains(GARBAGE_REFRESH_SECRET),
        "refresh secret leaked into error string: {msg}"
    );
    assert!(
        !msg.contains("refresh_token"),
        "raw token body must not appear in the error: {msg}"
    );
}

#[ntex::test]
async fn session_mint_recovers_after_reload_one_refresh() {
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
        let anchor_cookie =
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=").expect("anchor cookie");
        let anchor_pair = anchor_cookie.split(';').next().unwrap().to_string();

        let before = op.refresh_calls.load(Ordering::SeqCst);

        let req = test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("cookie", anchor_pair.clone())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "reload-recovery must succeed");

        let new_session_cookie = set_cookie_with_prefix(&resp, "__Host-zeroship_app_session=")
            .expect("reload-recovery must re-set the session cookie");
        assert!(new_session_cookie.contains("HttpOnly"));
        assert!(new_session_cookie.contains("SameSite=Lax"));
        let new_session_token = new_session_cookie
            .strip_prefix("__Host-zeroship_app_session=")
            .and_then(|rest| rest.split(';').next())
            .expect("session token in cookie")
            .to_string();
        let new_claims = state
            .session_verifier
            .as_ref()
            .expect("session verifier")
            .verify(&new_session_token, client_id())
            .expect("re-signed session cookie verifies locally");

        let body: serde_json::Value = read_json(resp).await;
        let expected_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id,
            &format!("https://{APP_HOST}"),
        );
        assert_eq!(body["user"]["id"], expected_pws);
        assert_ne!(body["user"]["id"], user_id.as_str());
        assert!(
            body.get("access_token").is_none(),
            "no access_token in reload-recovery body"
        );
        assert!(
            body.get("id_token").is_none(),
            "no id_token in reload-recovery body"
        );
        assert!(body["expires_at"].is_i64());
        assert_eq!(body["user"]["email"], serde_json::json!(relay_email));
        let raw_body = serde_json::to_string(&body).unwrap();
        assert!(
            !raw_body.contains(REAL_EMAIL),
            "real email absent from reload-recovery body"
        );
        assert!(
            !raw_body.contains(user_id.as_str()),
            "global UUID absent from reload-recovery body"
        );

        assert_eq!(
            op.refresh_calls.load(Ordering::SeqCst),
            before + 1,
            "reload-recovery triggers exactly one OP refresh"
        );

        assert_eq!(
            body["user"]["name"], ROTATED_NAME,
            "the projection must carry the rotated id_token name"
        );
        assert_eq!(
            new_claims.name, ROTATED_NAME,
            "re-signed cookie carries the rotated id_token name"
        );
        assert_eq!(
            new_claims.avatar.as_deref(),
            Some(ROTATED_AVATAR),
            "re-signed cookie carries the rotated id_token avatar (not degraded to NULL)"
        );
        assert_eq!(
            new_claims.app,
            client_id(),
            "re-signed cookie binds to the route client_id"
        );

        {
            let rows = database
                .admin
                .query(
                    "SELECT name, avatar_url FROM zeroship.gateway_sessions \
                 WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL \
                 ORDER BY issued_at DESC LIMIT 1",
                    &[&user_id.as_str(), &APP_ID],
                )
                .await
                .expect("audit row query");
            let row = rows.first().expect("a fresh audit row was re-written");
            let name: Option<String> = row.get("name");
            let avatar: Option<String> = row.get("avatar_url");
            assert_eq!(
                name.as_deref(),
                Some(ROTATED_NAME),
                "audit row name = rotated"
            );
            assert_eq!(
                avatar.as_deref(),
                Some(ROTATED_AVATAR),
                "audit row avatar = rotated"
            );
        }
    })
    .await;
}

#[ntex::test]
async fn session_mint_persists_rotated_refresh_token_for_next_rotation() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        op.enforce_refresh_reuse_detection();
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
        assert_eq!(
            resp.status().as_u16(),
            200,
            "initial session mint must succeed"
        );
        let anchor_cookie =
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=").expect("anchor cookie");
        let anchor_pair = anchor_cookie.split(';').next().unwrap().to_string();
        let anchor_id =
            anchors::parse_anchor_cookie(&anchor_pair).expect("anchor id parses from cookie");

        let expiry = database
            .admin
            .query_one(
                "SELECT created_at, abs_expires_at FROM zeroship.app_session_anchors WHERE id = $1",
                &[&anchor_id],
            )
            .await
            .unwrap();
        let created_at: chrono::DateTime<chrono::Utc> = expiry.get("created_at");
        let expires_at: chrono::DateTime<chrono::Utc> = expiry.get("abs_expires_at");
        assert_eq!(
            expires_at - created_at,
            chrono::TimeDelta::days(anchors::ANCHOR_ABS_DAYS)
        );

        let req = test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("cookie", anchor_pair.clone())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "first rotation must succeed");
        assert_eq!(
            op.presented_refresh_tokens(),
            vec![INITIAL_REFRESH_TOKEN.to_string()],
            "first rotation must present the authorization-code exchange's refresh token"
        );

        {
            let pool = crate::db::checkout(&db_cfg).await.expect("pool");
            let mut conn = pool.acquire().await.expect("conn");
            let anchor = anchors::read_live(
                &mut conn,
                &AppId::parse(APP_ID).expect("fixed app id"),
                anchor_id,
            )
            .await
            .expect("read rotated anchor")
            .expect("anchor remains live after rotation");
            assert_eq!(anchor.created_at, created_at);
            assert_eq!(
                anchor.abs_expires_at, expires_at,
                "rotation must not slide the absolute expiry"
            );
            let aad =
                format!("zs-anchor-refresh:{}:{}", client_id(), user_id.as_str()).into_bytes();
            let plaintext = zeroship_core::crypto::decrypt(
                &state.anchor_enc_key,
                &aad,
                &anchor.refresh_token_enc,
            )
            .expect("decrypt stored refresh token");
            let stored = String::from_utf8(plaintext).expect("stored refresh token utf8");
            assert_eq!(
                stored, "rt_rotated_1",
                "the anchor row must persist the OP's newly returned refresh token"
            );
        }

        let req = test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("cookie", anchor_pair)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status().as_u16(),
            200,
            "second rotation must use the rotated refresh token"
        );
        assert_eq!(
            op.presented_refresh_tokens(),
            vec![
                INITIAL_REFRESH_TOKEN.to_string(),
                "rt_rotated_1".to_string()
            ],
            "second rotation must present the first rotation's NEW refresh token"
        );
    })
    .await;
}

#[ntex::test]
async fn session_mint_invalid_grant_deletes_anchor_and_requires_login() {
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
        assert_eq!(
            resp.status().as_u16(),
            200,
            "initial session mint must succeed"
        );
        let anchor_cookie =
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=").expect("anchor cookie");
        let anchor_pair = anchor_cookie.split(';').next().unwrap().to_string();
        let anchor_id =
            anchors::parse_anchor_cookie(&anchor_pair).expect("anchor id parses from cookie");

        op.invalid_grant.store(true, Ordering::SeqCst);
        let req = test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("cookie", anchor_pair)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status().as_u16(),
            401,
            "invalid_grant must require login"
        );
        assert!(
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_session=").is_none(),
            "invalid_grant must not mint a fresh live session cookie"
        );
        let clear_anchor = set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=")
            .expect("anchor clear cookie");
        assert!(
            clear_anchor.contains("Max-Age=0"),
            "anchor cookie must be cleared on invalid_grant: {clear_anchor}"
        );
        let clear_breadcrumb =
            set_cookie_with_prefix(&resp, "zs.myapp.zeroship.ai.is.authenticated=")
                .expect("breadcrumb clear cookie");
        assert!(
            clear_breadcrumb.contains("Max-Age=0"),
            "breadcrumb must be cleared on invalid_grant: {clear_breadcrumb}"
        );
        let body = read_json(resp).await;
        assert_eq!(body["error"], "login_required");
        assert_eq!(
            op.presented_refresh_tokens(),
            vec![INITIAL_REFRESH_TOKEN.to_string()],
            "invalid_grant path must have attempted exactly one OP refresh"
        );

        {
            let pool = crate::db::checkout(&db_cfg).await.expect("pool");
            let mut conn = pool.acquire().await.expect("conn");
            let anchor = anchors::read_live(
                &mut conn,
                &AppId::parse(APP_ID).expect("fixed app id"),
                anchor_id,
            )
            .await
            .expect("read anchor after invalid_grant");
            assert!(
                anchor.is_none(),
                "invalid_grant must delete the server-held reload-recovery anchor"
            );
        }
    })
    .await;
}
