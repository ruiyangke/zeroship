use super::*;
use ed25519_dalek::SigningKey;

#[ntex::test]
async fn invalid_logout_tokens_leave_sessions_and_replay_state_untouched() {
    Database::migrated(async |database| {
        let handler = Handler::new(database, 1).await;
        let session = handler.session(&handler.user, &target_app(), None).await;
        let valid = handler.claims(Some(&handler.user), None);
        let mut wrong_audience = valid.clone();
        wrong_audience["aud"] = json!(zeroship_core::typed_id::app_oauth_client_id(&AppId::mint()));
        let mut missing_events = valid.clone();
        missing_events.as_object_mut().unwrap().remove("events");
        let mut wrong_subject = valid.clone();
        wrong_subject["sub"] = json!(AppId::mint().as_str());
        let mut wrong_issuer = valid.clone();
        wrong_issuer["iss"] = json!("https://foreign-provider.test");
        let bad_signature = Handler::sign_with(&SigningKey::from_bytes(&[10; 32]), &valid);
        let app = test::init_service(
            web::App::new()
                .state(handler.state.clone())
                .configure(crate::backchannel_logout::configure),
        )
        .await;
        for (label, token) in [
            ("wrong audience", handler.sign(&wrong_audience)),
            ("missing events", handler.sign(&missing_events)),
            ("wrong subject entity", handler.sign(&wrong_subject)),
            ("wrong issuer", handler.sign(&wrong_issuer)),
            ("bad signature", bad_signature),
        ] {
            let response = test::call_service(&app, request(&token)).await;
            assert_response(&response, StatusCode::BAD_REQUEST);
            assert_eq!(
                test::read_body(response).await.as_ref(),
                b"invalid logout_token",
                "{label}"
            );
            assert!(!revoked(&database.admin, session).await, "{label}");
            assert!(handler.state.logout_jti_cache.is_empty(), "{label}");
            assert_eq!(
                audit_count(&database.admin, valid["jti"].as_str().unwrap()).await,
                0,
                "{label}"
            );
            assert!(markers(&database.admin).await.is_empty(), "{label}");
        }
        // The same identifier must remain usable with a correctly signed token.
        assert_response(
            &test::call_service(&app, request(&handler.sign(&valid))).await,
            StatusCode::OK,
        );
        assert!(revoked(&database.admin, session).await);
        assert_audit(&database.admin, &valid, 1).await;
    })
    .await;
}
