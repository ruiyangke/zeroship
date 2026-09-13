use super::*;
use futures::{future::join_all, poll};
use std::collections::BTreeSet;
use std::time::Duration;

pub(crate) mod postgres;
use postgres::Database;

#[test]
fn db_config_clamps_pool_size_to_one() {
    let cfg = DbConfig::new("postgres://x", 0);
    assert_eq!(cfg.pool_size(), 1);
    let cfg = DbConfig::new("postgres://x", 32);
    assert_eq!(cfg.pool_size(), 32);
}

#[test]
fn db_config_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DbConfig>();
}

#[compio::test]
async fn concurrent_first_checkouts_converge_on_the_cached_pool() {
    Database::run(async |database| {
        let config = database.config(2);
        let mut first = Box::pin(checkout(&config));
        let mut second = Box::pin(checkout(&config));
        assert!(
            poll!(&mut first).is_pending(),
            "first checkout must connect"
        );
        assert!(
            poll!(&mut second).is_pending(),
            "competing checkout must connect"
        );
        let (first, second) = futures::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();
        let cached = checkout(&config).await.unwrap();
        assert!(
            Rc::ptr_eq(&first, &second),
            "concurrent callers installed different pools"
        );
        assert!(
            Rc::ptr_eq(&first, &cached),
            "subsequent checkout rebuilt the pool"
        );
        let leases = join_all([first.acquire(), second.acquire()]).await;
        let pids: BTreeSet<i32> = join_all(leases.iter().map(|lease| async {
            lease
                .as_ref()
                .unwrap()
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0)
        }))
        .await
        .into_iter()
        .collect();
        assert_eq!(pids.len(), config.pool_size());
    })
    .await;
}

#[compio::test]
async fn exhausted_capacity_waits_until_a_lease_is_returned_and_reuses_its_backend() {
    Database::run(async |database| {
        let config = database.config(3);
        let pool = checkout(&config).await.unwrap();
        let mut leases = Vec::new();
        let mut original_pids = BTreeSet::new();
        for _ in 0..config.pool_size() {
            let lease = pool.acquire().await.unwrap();
            let pid: i32 = lease
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0);
            assert!(
                original_pids.insert(pid),
                "simultaneous leases shared a backend"
            );
            leases.push((lease, pid));
        }
        let mut waiting = Box::pin(pool.acquire());
        assert!(
            poll!(&mut waiting).is_pending(),
            "exhausted pool exceeded its configured capacity"
        );
        let (released, released_pid) = leases.pop().unwrap();
        drop(released);
        let reused = compio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("returned lease must wake a waiter")
            .unwrap();
        let reused_pid: i32 = reused
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(reused_pid, released_pid, "returned backend was replaced");
        drop(reused);
        drop(leases);

        let next = join_all((0..config.pool_size()).map(|_| pool.acquire())).await;
        let next_pids: BTreeSet<i32> = join_all(next.iter().map(|lease| async {
            lease
                .as_ref()
                .unwrap()
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0)
        }))
        .await
        .into_iter()
        .collect();
        assert_eq!(
            next_pids, original_pids,
            "the next wave must reuse the returned backends"
        );
    })
    .await;
}

#[compio::test]
async fn checked_out_connections_execute_queries_concurrently() {
    Database::run(async |database| {
        let config = database.config(2);
        let pool = checkout(&config).await.unwrap();
        let leases = join_all((0..config.pool_size()).map(|_| pool.acquire())).await;
        let mut pids = Vec::new();
        for lease in &leases {
            pids.push(
                lease
                    .as_ref()
                    .unwrap()
                    .query_one("SELECT pg_backend_pid()", &[])
                    .await
                    .unwrap()
                    .get::<_, i32>(0),
            );
        }
        assert_eq!(
            pids.iter().collect::<BTreeSet<_>>().len(),
            config.pool_size()
        );
        let barrier = 711_i64;
        database
            .admin
            .query_one("SELECT pg_advisory_lock($1)", &[&barrier])
            .await
            .unwrap();
        let queries = join_all(leases.iter().zip(&pids).map(|(lease, pid)| async {
            let row = lease
                .as_ref()
                .unwrap()
                .query_one(
                    "SELECT pg_advisory_xact_lock_shared($1), $2::int, pg_backend_pid()",
                    &[&barrier, pid],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, i32>(1), *pid);
            assert_eq!(row.get::<_, i32>(2), *pid);
        }));
        let release = async {
            database.wait_until_blocked(&pids).await;
            let unlocked: bool = database
                .admin
                .query_one("SELECT pg_advisory_unlock($1)", &[&barrier])
                .await
                .unwrap()
                .get(0);
            assert!(unlocked);
        };
        futures::join!(queries, release);
    })
    .await;
}

#[compio::test]
async fn changing_the_database_replaces_the_cache_without_breaking_existing_leases() {
    Database::run(async |database| {
        let first = checkout(&database.config(1)).await.unwrap();
        let existing = first.acquire().await.unwrap();
        database
            .admin
            .batch_execute("CREATE DATABASE rotated")
            .await
            .unwrap();
        let mut next_url = database.url.clone();
        next_url.set_path("/rotated");
        let next_config = DbConfig::new(next_url.as_str(), 1);
        let next = checkout(&next_config).await.unwrap();
        assert!(
            !Rc::ptr_eq(&first, &next),
            "a changed database reused the old pool"
        );
        assert_eq!(
            next.query("SELECT current_database()", &[]).await.unwrap()[0].get::<_, String>(0),
            "rotated"
        );
        assert_eq!(
            existing
                .query_one("SELECT current_database()", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "gateway_tests"
        );
        assert!(Rc::ptr_eq(&next, &checkout(&next_config).await.unwrap()));
        drop(existing);
        first.close().await;
    })
    .await;
}

#[compio::test]
async fn a_failed_initial_connection_can_retry_after_the_database_is_created() {
    Database::run(async |database| {
        let mut url = database.url.clone();
        url.set_path("/created_later");
        let config = DbConfig::new(url.as_str(), 1);
        let error = checkout(&config).await.unwrap_err();
        assert_eq!(
            error.code(),
            Some(&compio_postgres::error::SqlState::INVALID_CATALOG_NAME)
        );
        database
            .admin
            .batch_execute("CREATE DATABASE created_later")
            .await
            .unwrap();
        let pool = checkout(&config)
            .await
            .expect("retry after creating the database");
        assert_eq!(
            pool.query("SELECT current_database()", &[]).await.unwrap()[0].get::<_, String>(0),
            "created_later"
        );
    })
    .await;
}
