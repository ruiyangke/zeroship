use super::*;
use std::sync::atomic::Ordering;

#[ntex::test]
async fn local_signout_revokes_the_minted_family_and_preserves_other_anchors() {
    exercise_signout("local", false).await;
}

#[ntex::test]
async fn global_signout_revokes_only_the_app_users_families() {
    exercise_signout("global", false).await;
}

#[ntex::test]
async fn provider_failure_does_not_undo_local_signout() {
    exercise_signout("local", true).await;
}

async fn stored_anchor_ids(admin: &compio_postgres::Client) -> Vec<Uuid> {
    admin
        .query(
            "SELECT id FROM zeroship.app_session_anchors ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn control_anchor(
    state: &GateState,
    app: &AppId,
    client: &str,
    user: &UserId,
    refresh: &str,
) -> Uuid {
    let encrypted = zeroship_core::crypto::encrypt(
        &state.anchor_enc_key,
        format!("zs-anchor-refresh:{client}:{}", user.as_str()).as_bytes(),
        refresh.as_bytes(),
    )
    .unwrap();
    let pool = crate::db::checkout(state.db.as_ref().unwrap())
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    anchors::create(
        &mut connection,
        &anchors::NewAnchor {
            app_id: app,
            client_id: client,
            global_user_id: user,
            refresh_token_enc: &encrypted,
            refresh_family_id: refresh,
            granted_scopes: &["openid".to_owned()],
        },
    )
    .await
    .unwrap()
    .id
}

async fn exercise_signout(scope: &str, provider_unavailable: bool) {
    Database::migrated(async |database| {
        let op = Arc::new(MockOP::new(client_id()));
        op.revoke_unavailable.store(provider_unavailable, Ordering::SeqCst);
        seed_user(&database.admin, &op.user_id).await;
        let (base, _provider) = boot_mock_op(op.clone()).await;
        let (state, _files) = build_state(&base, Some(database.config_as("zeroship_gateway", 1)));
        let app = test::init_service(browser_app!(state)).await;
        let target_app = AppId::parse(APP_ID).unwrap();

        // The real minter encrypts this family's refresh token and issues its cookie.
        let login = test::call_service(&app, test::TestRequest::post()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("content-type", "application/x-www-form-urlencoded")
            .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
            .to_request()).await;
        assert_eq!(login.status().as_u16(), 200);
        let anchor_cookie = set_cookie_with_prefix(&login, "__Host-zeroship_app_anchor=").unwrap();
        let primary = anchors::parse_anchor_cookie(anchor_cookie.split(';').next().unwrap()).unwrap();
        let session_cookie = set_cookie_with_prefix(&login, "__Host-zeroship_app_session=").unwrap();
        let read_session = || test::TestRequest::get()
            .uri("/__zeroship/auth/session")
            .header("host", APP_HOST)
            .header("cookie", session_cookie.split(';').next().unwrap())
            .to_request();
        assert_eq!(test::call_service(&app, read_session()).await.status().as_u16(), 200);

        let sibling = control_anchor(&state, &target_app, client_id(), &op.user_id, "rt_other_device").await;
        let other_user = UserId::mint();
        database.admin.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, 'other@zeroship.test', 'Other User')",
            &[&other_user.as_str()],
        ).await.unwrap();
        let other_users_anchor = control_anchor(&state, &target_app, client_id(), &other_user, "rt_other_user").await;
        let other_app = AppId::mint();
        let other_client = zeroship_core::typed_id::app_oauth_client_id(&other_app);
        database.admin.execute(
            "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
             SELECT $1, 'other-app', project_id, organization_id FROM zeroship.apps WHERE id = $2",
            &[&other_app.as_str(), &APP_ID],
        ).await.unwrap();
        database.admin.execute(
            "INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes) \
             VALUES ($1, 'Other App', ARRAY['https://other.zeroship.ai/__zeroship/auth/callback'], ARRAY['openid'])",
            &[&other_client],
        ).await.unwrap();
        let foreign = control_anchor(&state, &other_app, &other_client, &op.user_id, "rt_other_app").await;

        let request = |id: Uuid| test::TestRequest::post()
            .uri("/__zeroship/auth/signout")
            .header("host", APP_HOST)
            .header("origin", format!("https://{APP_HOST}"))
            .header("x-zs-auth", "1")
            .header("content-type", "application/json")
            .header("cookie", format!("__Host-zeroship_app_anchor={id}"))
            .set_payload(serde_json::json!({"scope": scope}).to_string())
            .to_request();

        // A foreign anchor must not grant authority over either app's families.
        let before = stored_anchor_ids(&database.admin).await;
        let refused = test::call_service(&app, request(foreign)).await;
        assert_eq!(refused.status().as_u16(), 204);
        assert_cookie_clears(&refused);
        assert_eq!(stored_anchor_ids(&database.admin).await, before);
        assert!(op.revoked_refresh_tokens.lock().unwrap().is_empty());
        assert!(database.admin.query("SELECT client_id FROM zeroship.token_revocations", &[]).await.unwrap().is_empty());

        let subject = test_pairwise_subject(&op.user_id, APP_HOST);
        let now = std::time::Instant::now();
        state.revocation_cache.store(client_id(), &subject, None, now);
        assert_eq!(state.revocation_cache.get(client_id(), &subject, now), Some(None));

        let response = test::call_service(&app, request(primary)).await;
        assert_eq!(response.status().as_u16(), 204);
        assert_cookie_clears(&response);
        assert_eq!(header_str(&response, "cache-control").as_deref(), Some("no-store"));
        let mut expected = vec![foreign, other_users_anchor];
        if scope == "local" { expected.push(sibling); }
        expected.sort_unstable();
        assert_eq!(stored_anchor_ids(&database.admin).await, expected,
            "signout must preserve anchors outside its scope");

        let mut revoked = op.revoked_refresh_tokens.lock().unwrap().clone();
        revoked.sort_unstable();
        let mut expected_tokens = vec![INITIAL_REFRESH_TOKEN.to_owned()];
        if scope == "global" { expected_tokens.push("rt_other_device".to_owned()); }
        expected_tokens.sort_unstable();
        assert_eq!(revoked, expected_tokens, "revoke the decrypted families with their broker credentials");
        let markers: Vec<(String, String)> = database.admin.query(
            "SELECT client_id, sub FROM zeroship.token_revocations", &[],
        ).await.unwrap().iter().map(|row| (row.get(0), row.get(1))).collect();
        assert_eq!(markers, [(client_id().to_owned(), subject.clone())]);
        assert_eq!(state.revocation_cache.get(client_id(), &subject, std::time::Instant::now()), None);
        let refused = test::call_service(&app, read_session()).await;
        assert_eq!(refused.status().as_u16(), 401);
        assert_eq!(read_json(refused).await["error"], "login_required");

        let repeated = test::call_service(&app, request(primary)).await;
        assert_eq!(repeated.status().as_u16(), 204);
        assert_cookie_clears(&repeated);
        assert_eq!(stored_anchor_ids(&database.admin).await, expected);
        let mut after_repeat = op.revoked_refresh_tokens.lock().unwrap().clone();
        after_repeat.sort_unstable();
        assert_eq!(after_repeat, expected_tokens, "repeated signout must not revoke other families");
    }).await;
}

#[ntex::test]
async fn signout_rejects_missing_custom_header_and_foreign_origin() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/signout")
        .header(http::header::HOST, APP_HOST)
        .header(http::header::ORIGIN, format!("https://{APP_HOST}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400, "missing X-ZS-Auth must 400");

    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/signout")
        .header(http::header::HOST, APP_HOST)
        .header("x-zs-auth", "1")
        .header(http::header::ORIGIN, "https://evil.example")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403, "foreign Origin must 403");
}

#[ntex::test]
async fn signout_without_an_anchor_clears_cookies() {
    let (state, _files) = state();
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/signout")
        .header(http::header::HOST, APP_HOST)
        .header("x-zs-auth", "1")
        .header(http::header::ORIGIN, format!("https://{APP_HOST}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 204, "signout must 204");
    assert_cookie_clears(&resp);
    assert_eq!(
        header_str(&resp, "cache-control").as_deref(),
        Some("no-store")
    );
}
