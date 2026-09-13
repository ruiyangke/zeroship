use super::*;

#[ntex::test]
#[allow(
    clippy::too_many_lines,
    reason = "follow the real cross-service logout from login through failed recovery"
)]
async fn revoking_an_app_session_at_the_provider_ends_gateway_authentication() {
    Database::migrated(async |database| {
        let redirect = format!("{SECTOR}/__zeroship/auth/popup-callback");
        let seeded = App::seed(database, &redirect).await;
        let provider = Provider::start(database).await;
        let gateway = Gateway::start(database, &provider, &seeded).await;
        let bcl_state = gateway.state.clone();
        let receiver = test::server(move || {
            let state = bcl_state.clone();
            async move { web::App::new().state(state).configure(crate::backchannel_logout::configure) }
        }).await;
        database.admin.execute(
            "UPDATE zeroship.oauth_clients SET backchannel_logout_uri = $2 WHERE client_id = $1",
            &[&seeded.client, &receiver.url("/oidc/backchannel-logout")],
        ).await.unwrap();
        let app = test::init_service(web::App::new().state(gateway.state.clone())
            .service(web::resource("/__zeroship/auth/session")
                .route(web::post().to(crate::auth_token::session_post))
                .route(web::get().to(crate::auth_token::session)))
            .service(web::resource("/{tail}*").route(web::route().to(crate::router::handle_subdomain)))).await;
        let (login, verifier) = browser_login(&provider, &seeded, &redirect).await;
        let minted = test::call_service(&app, test::TestRequest::post()
            .uri("/__zeroship/auth/session").header("host", APP_HOST).header("origin", SECTOR)
            .header("x-zs-auth", "1").header("content-type", "application/x-www-form-urlencoded")
            .set_payload(form(&[("grant_type", "authorization_code"), ("code", &login.code),
                ("code_verifier", &verifier), ("redirect_uri", &redirect)]))
            .to_request()).await;
        assert_eq!(minted.status(), StatusCode::OK);
        let cookie = cookie_pair(&minted, "__Host-zeroship_app_session=");
        let anchor = cookie_pair(&minted, "__Host-zeroship_app_anchor=");
        let anchor_id = crate::anchors::parse_anchor_cookie(&anchor).unwrap();
        let before = test::call_service(&app, protected_request(&cookie)).await;
        assert_eq!(before.status(), StatusCode::OK);
        assert_eq!(test::read_body(before).await.as_ref(), b"protected asset");
        let session: Uuid = database.admin.query_one(
            "SELECT id FROM zeroship.gateway_sessions WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL",
            &[&seeded.user.as_str(), &seeded.id.as_str()],
        ).await.expect("real login wrote the app session").get(0);
        assert!(database.admin.query_one("SELECT id FROM zeroship.app_session_anchors WHERE id = $1", &[&anchor_id])
            .await.is_ok(), "real login wrote the reload anchor");

        let csrf = "provider-revoke-csrf";
        let revoked = cyper::Client::new().post(format!("{}/me/sessions/{session}/revoke", provider.base)).unwrap()
            .header("content-type", "application/x-www-form-urlencoded").unwrap()
            .header("cookie", format!("__Host-zsidp_session={}; __Host-zsidp_csrf={csrf}", login.idp_session)).unwrap()
            .body(form(&[("csrf", csrf), ("kind", "app")])).send().await.unwrap();
        assert_eq!(revoked.status().as_u16(), 200);
        let body: Value = serde_json::from_slice(&revoked.bytes().await.unwrap()).unwrap();
        assert_eq!(body["revoked"], true);
        let after = test::call_service(&app, protected_request(&cookie)).await;
        assert_eq!(after.status(), StatusCode::UNAUTHORIZED,
            "the same cookie must no longer authenticate the protected route");
        let missing: bool = database.admin.query_one(
            "SELECT NOT EXISTS (SELECT FROM zeroship.app_session_anchors WHERE id = $1)", &[&anchor_id],
        ).await.unwrap().get(0);
        assert!(missing, "the real backchannel receiver must delete the reload anchor");
        let audit: Value = database.admin.query_one(
            "SELECT detail FROM zeroship.audit_events WHERE event_type = 'backchannel_logout_revoke' AND client_id = $1",
            &[&seeded.client],
        ).await.expect("the provider's HTTP logout reached the gateway receiver").get(0);
        assert_eq!(audit["sub"], seeded.user.as_str());
        let recovery = test::call_service(&app, test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1").header("host", APP_HOST).header("origin", SECTOR)
            .header("x-zs-auth", "1").header("cookie", &anchor).to_request()).await;
        assert_eq!(recovery.status(), StatusCode::UNAUTHORIZED);
        let cleared = crate::tests::browser::set_cookie_with_prefix(&recovery, "__Host-zeroship_app_session=");
        assert!(cleared.is_none(), "a revoked anchor must not issue a fresh session cookie");
        assert_eq!(crate::tests::browser::read_json(recovery).await["error"], "login_required");
    }).await;
}
