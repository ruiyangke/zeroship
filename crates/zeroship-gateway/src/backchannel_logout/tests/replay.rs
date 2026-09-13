use super::*;
use futures::{FutureExt, join, poll};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

#[ntex::test]
async fn completed_replay_preserves_new_sessions_and_the_original_audit() {
    Database::migrated(async |database| {
        let handler = Handler::new(database, 1).await;
        let original = handler.session(&handler.user, &target_app(), None).await;
        let claims = handler.claims(Some(&handler.user), None);
        let token = handler.sign(&claims);
        let app = test::init_service(
            web::App::new()
                .state(handler.state.clone())
                .configure(crate::backchannel_logout::configure),
        )
        .await;
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::OK,
        );
        assert!(revoked(&database.admin, original).await);
        assert_audit(&database.admin, &claims, 1).await;

        let later = handler.session(&handler.user, &target_app(), None).await;
        assert_response(
            &test::call_service(&app, request(&token)).await,
            StatusCode::OK,
        );
        assert!(
            !revoked(&database.admin, later).await,
            "a completed replay must not revoke a later login"
        );
        assert_audit(&database.admin, &claims, 1).await;
    })
    .await;
}

#[ntex::test]
async fn concurrent_replay_does_not_repeat_the_inflight_revocation() {
    exercise_concurrent_replay(false).await;
}

#[ntex::test]
async fn replay_waiting_for_the_pool_observes_the_completed_revocation() {
    exercise_concurrent_replay(true).await;
}

async fn exercise_concurrent_replay(queue_for_pool: bool) {
    Database::migrated(async |database| {
        let handler = Handler::new(database, if queue_for_pool { 1 } else { 2 }).await;
        let session = handler.session(&handler.user, &target_app(), None).await;
        let claims = handler.claims(Some(&handler.user), None);
        let token = handler.sign(&claims);
        handler.warm_jwks(&token).await;
        let pool = crate::db::checkout(handler.state.db.as_ref().unwrap())
            .await
            .unwrap();
        let conn = pool.acquire().await.unwrap();
        let pid: i32 = conn
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        drop(conn);
        let app = test::init_service(
            web::App::new()
                .state(handler.state.clone())
                .configure(crate::backchannel_logout::configure),
        )
        .await;
        database.admin.batch_execute("BEGIN").await.unwrap();
        database
            .admin
            .query_one(
                "SELECT id FROM zeroship.gateway_sessions WHERE id = $1 FOR UPDATE",
                &[&session],
            )
            .await
            .unwrap();
        let (finished_tx, finished_rx) = flume::bounded(1);
        let outcome = AssertUnwindSafe(async {
            let first = async {
                let response = test::call_service(&app, request(&token)).await;
                finished_tx.send(()).unwrap();
                response
            };
            let duplicate = async {
                database.wait_until_blocked(&[pid]).await;
                let mut pending = Box::pin(test::call_service(&app, request(&token)));
                if queue_for_pool {
                    assert!(
                        poll!(&mut pending).is_pending(),
                        "the duplicate must wait for the occupied pool"
                    );
                    database.admin.batch_execute("ROLLBACK").await.unwrap();
                    finished_rx.recv_async().await.unwrap();
                    pending.await
                } else {
                    let response = compio::time::timeout(Duration::from_secs(5), pending)
                        .await
                        .expect(
                            "inflight duplicate must finish while the original holds its claim",
                        );
                    assert_eq!(
                        audit_count(&database.admin, claims["jti"].as_str().unwrap()).await,
                        0
                    );
                    database.admin.batch_execute("ROLLBACK").await.unwrap();
                    response
                }
            };
            join!(first, duplicate)
        })
        .catch_unwind()
        .await;
        // Release the observer's lock even if the concurrency assertion panics.
        database.admin.batch_execute("ROLLBACK").await.unwrap();
        let (first, duplicate) = outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        assert_response(&first, StatusCode::OK);
        assert_response(&duplicate, StatusCode::OK);
        assert!(revoked(&database.admin, session).await);
        assert_audit(&database.admin, &claims, 1).await;
    })
    .await;
}
