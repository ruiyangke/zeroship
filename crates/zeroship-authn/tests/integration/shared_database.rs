//! The shared database fixture's own contract: one server for every case, a
//! failed case re-raises its own panic, and its connections are joined before it
//! does.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use crate::support::database::Database;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use zeroship_core::UserId;

#[compio::test]
async fn a_failed_case_re_raises_its_failure_and_closes_its_connections() {
    let shared = zeroship_testkit::postgres::server_container_id();
    let role = format!("authn_failed_{}", UserId::mint().as_str());
    let failure = AssertUnwindSafe(Database::run(async |database| {
        let admin = database.connect().await;
        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"
            ))
            .await
            .unwrap();
        let _case = database.connect_as(&role).await;
        let open: i64 = admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE usename = $1",
                &[&role],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            open > 0,
            "the failed case must hold its own connection before it unwinds"
        );
        panic!("intentional fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("a failed case must re-raise its own panic");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional fixture failure")
    );

    let mut open_after = None;
    Database::run(async |database| {
        let admin = database.connect().await;
        open_after = Some(
            admin
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE usename = $1",
                    &[&role],
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
        );
        admin
            .batch_execute(&format!("DROP ROLE \"{role}\""))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        open_after,
        Some(0),
        "a failed case must close its connections before re-raising"
    );
    assert_eq!(
        zeroship_testkit::postgres::server_container_id(),
        shared,
        "every case must reach the one shared server"
    );
}
