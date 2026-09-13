use super::*;

#[ntex::test]
async fn rejected_database_write_does_not_consume_the_logout_token() {
    exercise_database_failure(false).await;
}

#[ntex::test]
async fn sid_only_retry_finishes_teardown_after_sessions_were_already_revoked() {
    exercise_database_failure(true).await;
}

async fn exercise_database_failure(sid_only: bool) {
    Database::migrated(async |database| {
        let handler = Handler::new(database, 1).await;
        let session = handler
            .session(&handler.user, &target_app(), Some("target-sid"))
            .await;
        let claims = if sid_only {
            handler.claims(None, Some("target-sid"))
        } else {
            handler.claims(Some(&handler.user), None)
        };
        let token = handler.sign(&claims);
        let app = test::init_service(
            web::App::new()
                .state(handler.state.clone())
                .configure(crate::backchannel_logout::configure),
        )
        .await;
        database
            .admin
            .batch_execute("REVOKE INSERT ON zeroship.token_revocations FROM zeroship_gateway")
            .await
            .unwrap();
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::SERVICE_UNAVAILABLE,
        );
        assert!(handler.state.logout_jti_cache.is_empty());
        assert_eq!(
            audit_count(&database.admin, claims["jti"].as_str().unwrap()).await,
            0
        );
        assert!(markers(&database.admin).await.is_empty());
        assert_eq!(
            revoked(&database.admin, session).await,
            sid_only,
            "sid revocation commits before family teardown; subject revocation follows it"
        );

        database
            .admin
            .batch_execute("GRANT INSERT ON zeroship.token_revocations TO zeroship_gateway")
            .await
            .unwrap();
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::OK,
        );
        assert!(revoked(&database.admin, session).await);
        assert_eq!(
            markers(&database.admin).await,
            [(
                client_id().to_owned(),
                test_pairwise_subject(&handler.user, APP_HOST)
            )]
        );
        assert_audit(&database.admin, &claims, 1).await;
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::OK,
        );
        assert_audit(&database.admin, &claims, 1).await;
    })
    .await;
}

#[ntex::test]
async fn missing_sid_without_subject_can_be_retried_after_the_session_arrives() {
    Database::migrated(async |database| {
        let handler = Handler::new(database, 1).await;
        let existing = handler
            .session(&handler.user, &target_app(), Some("other-sid"))
            .await;
        let claims = handler.claims(None, Some("arriving-sid"));
        let token = handler.sign(&claims);
        let app = test::init_service(
            web::App::new()
                .state(handler.state.clone())
                .configure(crate::backchannel_logout::configure),
        )
        .await;
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::SERVICE_UNAVAILABLE,
        );
        assert!(!revoked(&database.admin, existing).await);
        assert!(handler.state.logout_jti_cache.is_empty());
        assert_eq!(
            audit_count(&database.admin, claims["jti"].as_str().unwrap()).await,
            0
        );
        assert!(markers(&database.admin).await.is_empty());

        let arriving = handler
            .session(&handler.user, &target_app(), Some("arriving-sid"))
            .await;
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::OK,
        );
        assert!(revoked(&database.admin, arriving).await);
        assert!(!revoked(&database.admin, existing).await);
        assert_audit(&database.admin, &claims, 1).await;
    })
    .await;
}
