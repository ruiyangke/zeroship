use super::*;

enum Scope {
    Subject,
    MatchingSid,
    MissingSid,
    SidOnly,
}

#[ntex::test]
async fn subject_revokes_session_rows_only_for_the_target_app_and_user() {
    exercise_scope(Scope::Subject).await;
}

#[ntex::test]
async fn matching_sid_limits_session_rows_to_the_named_session_and_subject() {
    exercise_scope(Scope::MatchingSid).await;
}

#[ntex::test]
async fn missing_sid_falls_back_to_all_session_rows_for_the_app_subject() {
    exercise_scope(Scope::MissingSid).await;
}

#[ntex::test]
async fn sid_only_resolves_the_subject_from_the_matching_session_rows() {
    exercise_scope(Scope::SidOnly).await;
}

async fn exercise_scope(scope: Scope) {
    Database::migrated(async |database| {
        let handler = Handler::new(database, 1).await;
        let target = handler
            .session(&handler.user, &target_app(), Some("target-sid"))
            .await;
        let sibling = handler
            .session(&handler.user, &target_app(), Some("sibling-sid"))
            .await;
        let foreign_app = other_app(&database.admin).await;
        let foreign = handler
            .session(&handler.user, &foreign_app, Some("target-sid"))
            .await;
        let another_user = other_user(&database.admin).await;
        let another_sid = if matches!(scope, Scope::MatchingSid) {
            "target-sid"
        } else {
            "another-user-sid"
        };
        let another = handler
            .session(&another_user, &target_app(), Some(another_sid))
            .await;
        for id in [target, sibling, foreign, another] {
            assert!(!revoked(&database.admin, id).await);
        }
        assert!(markers(&database.admin).await.is_empty());

        let claims = match scope {
            Scope::Subject => handler.claims(Some(&handler.user), None),
            Scope::MatchingSid => handler.claims(Some(&handler.user), Some("target-sid")),
            Scope::MissingSid => handler.claims(Some(&handler.user), Some("missing-sid")),
            Scope::SidOnly => handler.claims(None, Some("target-sid")),
        };
        let app = test::init_service(
            web::App::new()
                .state(handler.state.clone())
                .configure(crate::backchannel_logout::configure),
        )
        .await;
        assert_response(
            &test::call_service(&app, request(&handler.sign(&claims))).await,
            StatusCode::OK,
        );
        assert!(revoked(&database.admin, target).await);
        let all_subject_rows = matches!(scope, Scope::Subject | Scope::MissingSid);
        assert_eq!(revoked(&database.admin, sibling).await, all_subject_rows);
        assert!(
            !revoked(&database.admin, foreign).await,
            "preserve the same user and sid at another app"
        );
        assert!(
            !revoked(&database.admin, another).await,
            "preserve an unrelated user's session row"
        );
        assert_eq!(
            markers(&database.admin).await,
            [(
                client_id().to_owned(),
                test_pairwise_subject(&handler.user, APP_HOST)
            )]
        );
        assert_audit(
            &database.admin,
            &claims,
            if all_subject_rows { 2 } else { 1 },
        )
        .await;
    })
    .await;
}
