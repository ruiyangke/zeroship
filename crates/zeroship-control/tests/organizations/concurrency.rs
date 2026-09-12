use super::{organizations, seed_user, Fx, Org, OrganizationError};
use compio_postgres::Client;
use std::time::Duration;

#[compio::test]
async fn concurrent_owner_departures_preserve_the_last_owner() {
    let fx = Fx::new().await;
    let mut org = Org::new(&fx, "concurrent-leave").await;
    let second = seed_user(&fx.pg, "co-owner").await;
    // The public transfer operation moves ownership rather than duplicating
    // it. Seed the co-owner state directly to exercise the last-owner fence.
    fx.pg
        .execute(
            "INSERT INTO zeroship.organization_members (organization_id, user_id, role)
             VALUES ($1, $2, 'owner')",
            &[&org.id, &second],
        )
        .await
        .unwrap();
    org.seeded.push(second);

    let (mut blocker, connection) =
        compio_postgres::connect(&super::common::require_control_db(), compio_postgres::NoTls)
            .await
            .unwrap();
    let driver = compio::runtime::spawn(async move { connection.run().await });
    let tx = blocker.transaction().await.unwrap();
    let blocker_pid: i32 = tx
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    tx.query_one(
        "SELECT id FROM zeroship.organizations WHERE id = $1 FOR UPDATE",
        &[&org.id],
    )
    .await
    .unwrap();

    let (queued, (first_result, second_result)) = compio::time::timeout(
        Duration::from_secs(60),
        Box::pin(async {
            futures::join!(
                async {
                    let queued = wait_for_departures(&fx.pg, blocker_pid).await;
                    tx.commit().await.unwrap();
                    queued
                },
                async {
                    futures::join!(
                        organizations::leave_organization(&fx.registry, org.owner, &org.id, None),
                        organizations::leave_organization(&fx.registry, second, &org.id, None),
                    )
                },
            )
        }),
    )
    .await
    .expect("concurrent departures must finish after the fixture releases its lock");
    let owners = org.owner_count(&fx).await;
    let first_role = org.role_of(&fx, org.owner).await;
    let second_role = org.role_of(&fx, second).await;
    org.cleanup(&fx).await;
    drop(blocker);
    compio::time::timeout(Duration::from_secs(10), driver)
        .await
        .expect("fixture connection must close")
        .expect("fixture connection task")
        .expect("fixture connection driver");

    assert!(
        queued,
        "both departures must wait on the fixture's organization lock"
    );
    assert_eq!(owners, 1);
    match (first_result, second_result) {
        (Ok(()), Err(OrganizationError::LastOwner)) => {
            assert_eq!(first_role, None);
            assert_eq!(second_role.as_deref(), Some("owner"));
        }
        (Err(OrganizationError::LastOwner), Ok(())) => {
            assert_eq!(first_role.as_deref(), Some("owner"));
            assert_eq!(second_role, None);
        }
        other => panic!("expected a committed departure and a last-owner refusal: {other:?}"),
    }
}

async fn wait_for_departures(observer: &Client, blocker_pid: i32) -> bool {
    compio::time::timeout(Duration::from_secs(15), async {
        loop {
            // Follow the owned session's blocking chain: PostgreSQL may queue
            // a departure behind its peer rather than directly behind us.
            let waiting: i64 = observer
                .query_one(
                    "WITH RECURSIVE blocked(pid) AS (
                         SELECT $1::integer
                         UNION
                         SELECT a.pid FROM pg_stat_activity a
                         JOIN blocked b ON b.pid = ANY(pg_blocking_pids(a.pid))
                         WHERE a.datname = current_database()
                     ) SELECT count(*)::bigint FROM blocked WHERE pid <> $1",
                    &[&blocker_pid],
                )
                .await
                .unwrap()
                .get(0);
            if waiting == 2 {
                return;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}
