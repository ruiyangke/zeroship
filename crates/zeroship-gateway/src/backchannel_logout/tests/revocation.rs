use super::*;
use crate::anchors;
use std::sync::atomic::Ordering;

#[ntex::test]
async fn logout_revokes_minted_credentials_and_preserves_other_apps_and_users() {
    exercise_revocation(false).await;
}

#[ntex::test]
async fn provider_revoke_failure_does_not_restore_the_deleted_anchor() {
    exercise_revocation(true).await;
}

#[allow(
    clippy::too_many_lines,
    reason = "follow the minted credential through logout and failed recovery in one scenario"
)]
async fn exercise_revocation(provider_unavailable: bool) {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        op.revoke_unavailable
            .store(provider_unavailable, Ordering::SeqCst);
        seed_user(&database.admin, &op.user_id).await;
        let (base, _provider) = boot_mock_op(op.clone()).await;
        let (state, _files) = build_state(&base, Some(database.config_as("zeroship_gateway", 1)));
        let app = test::init_service(anchors_bcl_app!(state)).await;
        let login = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/__zeroship/auth/session")
                .header("host", APP_HOST)
                .header("origin", format!("https://{APP_HOST}"))
                .header("x-zs-auth", "1")
                .header("content-type", "application/x-www-form-urlencoded")
                .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
                .to_request(),
        )
        .await;
        assert_eq!(login.status(), StatusCode::OK);
        let anchor_cookie = set_cookie_with_prefix(&login, "__Host-zeroship_app_anchor=").unwrap();
        let anchor_pair = anchor_cookie.split(';').next().unwrap();
        let primary = anchors::parse_anchor_cookie(anchor_pair).unwrap();
        let session_cookie =
            set_cookie_with_prefix(&login, "__Host-zeroship_app_session=").unwrap();
        let read_session = || {
            test::TestRequest::get()
                .uri("/__zeroship/auth/session")
                .header("host", APP_HOST)
                .header("cookie", session_cookie.split(';').next().unwrap())
                .to_request()
        };
        assert_eq!(
            test::call_service(&app, read_session()).await.status(),
            StatusCode::OK
        );
        assert!(stored_anchor_ids(&database.admin).await.contains(&primary));
        let session: Uuid = database
            .admin
            .query_one(
                "SELECT id FROM zeroship.gateway_sessions WHERE user_id = $1 AND app_id = $2",
                &[&op.user_id.as_str(), &APP_ID],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!revoked(&database.admin, session).await);

        let sibling = control_anchor(
            &state,
            &target_app(),
            client_id(),
            &op.user_id,
            "rt_other_device",
        )
        .await;
        let another_user = other_user(&database.admin).await;
        let another = control_anchor(
            &state,
            &target_app(),
            client_id(),
            &another_user,
            "rt_other_user",
        )
        .await;
        let foreign_app = other_app(&database.admin).await;
        let foreign_client = zeroship_core::typed_id::app_oauth_client_id(&foreign_app);
        database.admin.execute(
            "INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes) \
             VALUES ($1, 'Other App', ARRAY['https://other.zeroship.ai/cb'], ARRAY['openid'])",
            &[&foreign_client],
        ).await.unwrap();
        let foreign = control_anchor(
            &state,
            &foreign_app,
            &foreign_client,
            &op.user_id,
            "rt_other_app",
        )
        .await;
        let mut before = vec![primary, sibling, another, foreign];
        before.sort_unstable();
        assert_eq!(stored_anchor_ids(&database.admin).await, before);
        assert!(markers(&database.admin).await.is_empty());

        let subject = test_pairwise_subject(&op.user_id, APP_HOST);
        let now = std::time::Instant::now();
        state
            .revocation_cache
            .store(client_id(), &subject, None, now);
        assert_eq!(
            state.revocation_cache.get(client_id(), &subject, now),
            Some(None)
        );
        let jti = Uuid::new_v4().to_string();
        let token = op.logout_token(&jti);
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::OK,
        );
        assert!(revoked(&database.admin, session).await);
        let mut survivors = vec![another, foreign];
        survivors.sort_unstable();
        assert_eq!(
            stored_anchor_ids(&database.admin).await,
            survivors,
            "family teardown must preserve other apps and users"
        );
        let mut expected_tokens = vec![
            INITIAL_REFRESH_TOKEN.to_owned(),
            "rt_other_device".to_owned(),
        ];
        expected_tokens.sort_unstable();
        let mut revoked_tokens = op.revoked_refresh_tokens.lock().unwrap().clone();
        revoked_tokens.sort_unstable();
        assert_eq!(
            revoked_tokens, expected_tokens,
            "decrypt the minter's ciphertext and revoke with the app's broker credentials"
        );
        assert_eq!(
            markers(&database.admin).await,
            [(client_id().to_owned(), subject.clone())]
        );
        assert_eq!(
            state
                .revocation_cache
                .get(client_id(), &subject, std::time::Instant::now()),
            None
        );
        assert_eq!(
            test::call_service(&app, read_session()).await.status(),
            StatusCode::UNAUTHORIZED
        );

        let recovery = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/__zeroship/auth/session?mint=1")
                .header("host", APP_HOST)
                .header("origin", format!("https://{APP_HOST}"))
                .header("x-zs-auth", "1")
                .header("cookie", anchor_pair)
                .to_request(),
        )
        .await;
        assert_eq!(recovery.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(read_json(recovery).await["error"], "login_required");
        assert_eq!(
            op.refresh_calls.load(Ordering::SeqCst),
            0,
            "deleted anchors must not contact the provider to resurrect a session"
        );
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::OK,
        );
        assert_eq!(stored_anchor_ids(&database.admin).await, survivors);
        let mut after_replay = op.revoked_refresh_tokens.lock().unwrap().clone();
        after_replay.sort_unstable();
        assert_eq!(after_replay, expected_tokens);
        assert_eq!(audit_count(&database.admin, &jti).await, 1);
    })
    .await;
}
