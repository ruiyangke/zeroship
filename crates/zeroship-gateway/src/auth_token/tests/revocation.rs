use super::*;

#[ntex::test]
async fn backchannel_logout_revokes_refreshed_session_with_sid_logout_token() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(bcl_client_id()));
        op.omit_refresh_id_token_on_refresh();
        let user_id = op.user_id.clone();
        let _email = seed_app_and_client_for(
            &database.admin,
            &user_id,
            BCL_REFRESH_APP_ID,
            BCL_REFRESH_APP_NAME,
            BCL_REFRESH_APP_HOST,
            bcl_client_id(),
        )
        .await;
        let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
        seed_relay_alias_for(
            &database.admin,
            bcl_client_id(),
            BCL_REFRESH_APP_HOST,
            &user_id,
            &relay_email,
        )
        .await;
        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db_cfg = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state_with_route(
            &base,
            Some(db_cfg.clone()),
            BCL_REFRESH_APP_ID,
            BCL_REFRESH_APP_NAME,
            BCL_REFRESH_APP_HOST,
            bcl_client_id(),
        );
        let app = test::init_service(anchors_bcl_app!(state.clone())).await;
        let app_id = BCL_REFRESH_APP_ID;

        let req = test::TestRequest::post()
            .uri("/__zeroship/auth/session")
            .header("host", BCL_REFRESH_APP_HOST)
            .header("origin", format!("https://{BCL_REFRESH_APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "initial login must succeed");
        let anchor_cookie =
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=").expect("anchor cookie");
        let anchor_pair = anchor_cookie.split(';').next().unwrap().to_string();
        let anchor_id =
            anchors::parse_anchor_cookie(&anchor_pair).expect("anchor id parses from cookie");

        {
            let client = &database.admin;
            let row = client
                .query_one(
                    "SELECT sid FROM zeroship.gateway_sessions \
                 WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL \
                 ORDER BY issued_at DESC LIMIT 1",
                    &[&user_id.as_str(), &app_id],
                )
                .await
                .expect("initial session row");
            let sid: Option<String> = row.get("sid");
            assert_eq!(
                sid.as_deref(),
                Some(op.sid.as_str()),
                "initial login session must persist the OP sid"
            );
        }

        let req = test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1")
            .header("host", BCL_REFRESH_APP_HOST)
            .header("origin", format!("https://{BCL_REFRESH_APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("cookie", anchor_pair.clone())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "refresh rotation must succeed");
        assert_eq!(
            op.refresh_calls.load(Ordering::SeqCst),
            1,
            "test must exercise the real refresh-grant rotation path"
        );

        let refreshed_session_id = {
            let client = &database.admin;
            let row = client
                .query_one(
                    "SELECT id AS session_id, sid FROM zeroship.gateway_sessions \
                 WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL \
                 ORDER BY issued_at DESC LIMIT 1",
                    &[&user_id.as_str(), &app_id],
                )
                .await
                .expect("refreshed session row");
            let sid: Option<String> = row.get("sid");
            assert_eq!(
                sid.as_deref(),
                Some(op.sid.as_str()),
                "refreshed session row must preserve sid even though refresh returned no id_token"
            );
            row.get::<_, Uuid>("session_id")
        };

        let jti = format!("jti-{}", Uuid::new_v4().simple());
        let logout_token = op.logout_token(&jti);
        let req = test::TestRequest::post()
            .uri("/oidc/backchannel-logout")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload(format!("logout_token={logout_token}"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "valid BCL must succeed");
        assert_eq!(*op.revoked_refresh_tokens.lock().unwrap(), ["rt_rotated_1"]);

        {
            let pool = crate::db::checkout(&db_cfg).await.expect("pool");
            let mut conn = pool.acquire().await.expect("conn");
            let row = database
                .admin
                .query_one(
                    "SELECT revoked_at IS NOT NULL AS revoked \
                 FROM zeroship.gateway_sessions WHERE id = $1",
                    &[&refreshed_session_id],
                )
                .await
                .expect("refreshed session revoked_at");
            let revoked: bool = row.get("revoked");
            assert!(revoked, "BCL must revoke the refreshed gateway session row");
            let deleted: bool = database
                .admin
                .query_one(
                    "SELECT NOT EXISTS (SELECT FROM zeroship.app_session_anchors WHERE id = $1)",
                    &[&anchor_id],
                )
                .await
                .unwrap()
                .get(0);
            assert!(deleted, "BCL must delete the stored anchor");
            assert!(
                anchors::read_live(
                    &mut conn,
                    &AppId::parse(app_id).expect("fixed app id"),
                    anchor_id
                )
                .await
                .expect("post-BCL anchor read")
                .is_none(),
                "BCL must delete the reload-recovery anchor for the refreshed session"
            );
        }

        let req = test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1")
            .header("host", BCL_REFRESH_APP_HOST)
            .header("origin", format!("https://{BCL_REFRESH_APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("cookie", anchor_pair)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status().as_u16(),
            401,
            "deleted anchor must make /session?mint=1 require login, not re-mint"
        );
        assert!(
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_session=").is_none(),
            "post-BCL /session?mint=1 must not set a fresh session cookie"
        );
    })
    .await;
}

#[ntex::test]
async fn cookie_mint_writes_identity_so_reset_evicts_cookie_session() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        let user_id = op.user_id.clone();
        let email = seed_app_and_client(&database.admin, &user_id).await;

        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db_cfg = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db_cfg));
        let app = test::init_service(anchors_app!(state.clone())).await;

        let expected_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id,
            &format!("https://{APP_HOST}"),
        );

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
        assert_eq!(resp.status().as_u16(), 200, "real cookie mint must succeed");
        let cookie = set_cookie_with_prefix(&resp, "__Host-zeroship_app_session=")
            .expect("minted session cookie");
        let cookie_pair = cookie.split(';').next().unwrap();
        let read = || {
            test::TestRequest::get()
                .uri("/__zeroship/auth/session")
                .header("host", APP_HOST)
                .header("cookie", cookie_pair)
                .to_request()
        };
        assert_eq!(
            test::call_service(&app, read()).await.status().as_u16(),
            200
        );

        let persisted: String = database
            .admin
            .query_one(
                "SELECT pairwise_sub FROM zeroship.app_user_identities \
         WHERE app_client_id = $1 AND global_user_id = $2",
                &[&client_id(), &user_id.as_str()],
            )
            .await
            .expect("observe the identity written by the minter")
            .get(0);
        assert_eq!(persisted, expected_pws);

        let auth_client = database.connect_as("zeroship_auth").await;
        let issued = zeroship_auth::identity::password_reset::issue(&auth_client, &email)
            .await
            .expect("issue reset token");
        let new_hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$Zm9vYmFyZm9vYmFyZm9vYmFy";
        let completed =
            zeroship_auth::identity::password_reset::complete(&auth_client, &issued.raw, new_hash)
                .await
                .expect("complete reset")
                .expect("reset must resolve to the seeded user");
        assert_eq!(
            completed.user_id, user_id,
            "reset must target the seeded user"
        );

        let assert_client = &database.admin;
        let marker_count: i64 = assert_client
            .query_one(
                "SELECT COUNT(*) FROM zeroship.token_revocations \
             WHERE client_id = $1 AND sub = $2",
                &[&client_id(), &expected_pws],
            )
            .await
            .expect("count family markers")
            .get(0);
        assert_eq!(
            marker_count, 1,
            "password reset must record the cookie family cutoff"
        );

        let refused = test::call_service(&app, read()).await;
        assert_eq!(refused.status().as_u16(), 401);
        assert_eq!(read_json(refused).await["error"], "login_required");
    })
    .await;
}

#[ntex::test]
async fn interactive_cookie_mint_writes_identity_so_reset_evicts_session() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        let user_id = op.user_id.clone();
        let email = seed_app_and_client(&database.admin, &user_id).await;

        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db_cfg = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db_cfg.clone()));

        let sector = format!("https://{APP_HOST}");
        let expected_pws =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &user_id, &sector);

        let cookie_iat = now_secs();

        let cookie = crate::auth_token::issue_interactive_session_cookie(
            &state,
            &db_cfg,
            client_id(),
            Some(sector.as_str()),
            &user_id,
            cookie_iat,
            Some("Interactive User"),
            None,
            Some(true),
            Some(cookie_iat),
            &[],
            &["openid".to_string(), "email".to_string()],
        )
        .await
        .expect("interactive minter must succeed");
        assert!(
            cookie.starts_with("__Host-zeroship_app_session="),
            "interactive minter must set the live signed session cookie"
        );

        let persisted: String = database
            .admin
            .query_one(
                "SELECT pairwise_sub FROM zeroship.app_user_identities \
         WHERE app_client_id = $1 AND global_user_id = $2",
                &[&client_id(), &user_id.as_str()],
            )
            .await
            .expect("observe the identity written by the minter")
            .get(0);
        assert_eq!(persisted, expected_pws);

        let app = test::init_service(anchors_app!(state)).await;
        let read = || {
            test::TestRequest::get()
                .uri("/__zeroship/auth/session")
                .header("host", APP_HOST)
                .header("cookie", cookie.split(';').next().unwrap())
                .to_request()
        };
        assert_eq!(
            test::call_service(&app, read()).await.status().as_u16(),
            200
        );

        let auth_client = database.connect_as("zeroship_auth").await;
        let issued = zeroship_auth::identity::password_reset::issue(&auth_client, &email)
            .await
            .expect("issue reset token");
        let new_hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$Zm9vYmFyZm9vYmFyZm9vYmFy";
        let completed =
            zeroship_auth::identity::password_reset::complete(&auth_client, &issued.raw, new_hash)
                .await
                .expect("complete reset")
                .expect("reset must resolve to the seeded user");
        assert_eq!(
            completed.user_id, user_id,
            "reset must target the seeded user"
        );

        let refused = test::call_service(&app, read()).await;
        assert_eq!(refused.status().as_u16(), 401);
        assert_eq!(read_json(refused).await["error"], "login_required");
    })
    .await;
}

#[ntex::test]
async fn mint_racing_concurrent_reset_fails_closed_no_fresh_cookie() {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        let user_id = op.user_id.clone();
        let email = seed_app_and_client(&database.admin, &user_id).await;
        let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
        seed_relay_alias(&database.admin, &user_id, &relay_email).await;

        let (base, _srv) = boot_mock_op(op.clone()).await;
        let db_cfg = database.config_as("zeroship_gateway", 8);
        let (state, _files) = build_state(&base, Some(db_cfg));
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
        assert_eq!(resp.status().as_u16(), 200, "real cookie mint must succeed");
        let anchor_cookie =
            set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=").expect("anchor cookie");
        let anchor_pair = anchor_cookie.split(';').next().unwrap().to_string();

        let auth_client = database.connect_as("zeroship_auth").await;
        let issued = zeroship_auth::identity::password_reset::issue(&auth_client, &email)
            .await
            .expect("issue reset token");
        let new_hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$Zm9vYmFyZm9vYmFyZm9vYmFy";

        let (entered, release) = op.pause_refresh();

        let mint_fut = async {
            let req = test::TestRequest::get()
                .uri("/__zeroship/auth/session?mint=1")
                .header("host", APP_HOST)
                .header("origin", format!("https://{APP_HOST}"))
                .header("x-zs-auth", "1")
                .header("cookie", anchor_pair.clone())
                .to_request();
            test::call_service(&app, req).await
        };
        let reset_fut = async {
            entered.recv_async().await.expect("refresh reached the OP");
            let completed = zeroship_auth::identity::password_reset::complete(
                &auth_client,
                &issued.raw,
                new_hash,
            )
            .await
            .expect("complete reset")
            .expect("reset resolves to the seeded user");
            release.send(()).expect("resume rotation after reset");
            completed
        };
        let (mint_resp, completed) = futures::future::join(mint_fut, reset_fut).await;
        assert_eq!(completed.user_id, user_id, "reset targeted the seeded user");

        let status = mint_resp.status().as_u16();
        let fresh_session_cookie =
            set_cookie_with_prefix(&mint_resp, "__Host-zeroship_app_session=");
        assert!(
            fresh_session_cookie.is_none(),
            "F4: a ?mint=1 racing a concurrent reset MUST NOT re-sign a fresh session \
         cookie (got: {fresh_session_cookie:?}, status {status})"
        );
        assert_eq!(
        status, 401,
        "F4: the rotation must fail closed (login_required), not return a 200 identity projection"
    );

        let assert_client = &database.admin;
        let live_anchor_count: i64 = assert_client
            .query_one(
                "SELECT COUNT(*) FROM zeroship.app_session_anchors \
             WHERE global_user_id = $1 AND revoked_at IS NULL",
                &[&user_id.as_str()],
            )
            .await
            .expect("count live anchors")
            .get(0);
        assert_eq!(
            live_anchor_count, 0,
            "the concurrent reset must have revoked the anchor (genuine race)"
        );
        let expected_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id,
            &format!("https://{APP_HOST}"),
        );
        let marker_count: i64 = assert_client
            .query_one(
                "SELECT COUNT(*) FROM zeroship.token_revocations \
             WHERE client_id = $1 AND sub = $2",
                &[&client_id(), &expected_pws],
            )
            .await
            .expect("count markers")
            .get(0);
        assert_eq!(
            marker_count, 1,
            "the reset must have written the family marker"
        );
    })
    .await;
}
