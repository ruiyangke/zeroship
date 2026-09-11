//! PostgreSQL connections contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use compio_postgres::Pool;

/// The connection budget this whole binary is allowed to hold at once,
/// expressed as open sockets in the process.
///
/// Well under a stock server's `max_connections` of 100, and well under a
/// stock `RLIMIT_NOFILE` of 1024, so neither limit is what this trips on.
const SOCKET_CEILING: usize = 24;

/// Sockets this process currently has open.
fn open_sockets() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("procfs is required to count this process's sockets")
        .filter_map(Result::ok)
        .filter(|e| {
            std::fs::read_link(e.path())
                .is_ok_and(|target| target.to_string_lossy().starts_with("socket:"))
        })
        .count()
}

/// Repeated fixture owners must close their sockets before dropping their runtimes.
#[test]
fn connections_do_not_outlive_the_runtime_that_opened_them() {
    if !std::env::args().any(|argument| argument == "--exact") {
        let qualified = concat!(
            module_path!(),
            "::connections_do_not_outlive_the_runtime_that_opened_them"
        );
        let test_name = qualified.split_once("::").unwrap().1;
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .output()
            .expect("start isolated socket lifecycle test");
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("1 passed"),
            "the child must execute the lifecycle assertion"
        );
        return;
    }
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
    let baseline = open_sockets();
    for _ in 0..40 {
        let url = postgres.url();
        std::thread::spawn(move || {
            Host::test(|host| {
                host.run(async {
                    host.set_database_url(&url);
                    let backend = host.backend().await.unwrap();
                    backend
                        .get::<zeroship_data_orm::backend::PostgresBackend>()
                        .unwrap()
                        .pool()
                        .execute("SELECT 1", &[])
                        .await
                        .unwrap();
                })
            });
        })
        .join()
        .expect("fixture owner thread");
    }
    let leaked = open_sockets().saturating_sub(baseline);
    assert!(
        leaked <= SOCKET_CEILING,
        "fixture owners leaked {leaked} sockets"
    );
}

/// Teardown owns connection cleanup even when an assertion unwinds the test body.
#[test]
fn fixture_teardown_closes_connections_after_a_panic() {
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
    let url = postgres.url();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            Host::test(|host| {
                host.run(async {
                    host.set_database_url(&url);
                    let backend = host.backend().await.unwrap();
                    backend
                        .get::<zeroship_data_orm::backend::PostgresBackend>()
                        .unwrap()
                        .pool()
                        .execute("SELECT 1", &[])
                        .await
                        .unwrap();
                    panic!("intentional fixture failure");
                })
            })
        });
        assert!(result.is_err(), "the failing test body must unwind");
        assert_eq!(
            compio_postgres::live_connections(),
            0,
            "fixture teardown left a connection behind"
        );
    })
    .join()
    .expect("fixture owner thread");
}

#[test]
fn a_dedicated_client_is_a_pool_checkout_and_returns_on_drop() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                std::rc::Rc::clone(&pool),
                url.clone(),
                host.key_source(),
            );

            let active_before = pool.active_count();
            let created_before = pool.metrics().connections_created.get();

            let client = {
                use crate::tests::fixtures::DatabaseFixture;
                backend
                    .fixture_session("app_pool_probe")
                    .await
                    .expect("dedicated client")
            };

            assert_eq!(
                pool.active_count(),
                active_before + 1,
                "a dedicated client must be a checkout from THIS pool; the pool's \
         active count did not move, so the connection came from somewhere else"
            );
            // The warm pool already holds idle connections, so this checkout must not
            // have opened a new backend at all.
            assert_eq!(
                pool.metrics().connections_created.get(),
                created_before,
                "the checkout opened a new connection instead of reusing an idle one"
            );

            drop(client);

            assert_eq!(
                pool.active_count(),
                active_before,
                "the dedicated client did not return to the pool on drop"
            );
        })
    })
}

#[test]
fn concurrent_dedicated_clients_are_bounded_by_the_pool() {
    Host::test(|host| {
        host.run(async {
            use std::time::Duration;

            let (_postgres, url) = require_pg(host).await;
            // `max_size: 1` makes the ceiling observable in one checkout; a short
            // acquire timeout keeps the queued caller's wait bounded so the test is
            // measuring the ceiling rather than sitting on the 30 s default.
            let mut config = compio_postgres::PoolConfig::default();
            config
                .max_size(1)
                .min_idle(1)
                .acquire_timeout(Duration::from_millis(400));
            let pool = std::rc::Rc::new(
                Pool::connect_with_pool_config(&url, config)
                    .await
                    .expect("pool"),
            );
            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                std::rc::Rc::clone(&pool),
                url.clone(),
                host.key_source(),
            );

            use crate::tests::fixtures::DatabaseFixture;
            let first = backend
                .fixture_session("app_pool_probe")
                .await
                .expect("first dedicated client");

            // THE INVERSION THIS STEP OWNS: a transaction that used to get a
            // connection of its own now queues, and refuses when the wait expires.
            // Conservative policy, and OWED a real decision: queue on the pool's
            // acquire timeout rather than refuse immediately, no per-app fairness, and
            // the ceiling is whatever the shared data pool is sized to.
            let second = backend.fixture_session("app_pool_probe").await;
            let err = second.expect_err(
                "a second dedicated client must be bounded by the pool, not opened \
         directly - an unbounded model is how one worker exhausts max_connections",
            );
            let message = format!("{err:?}");
            assert!(
                message.contains("acquisition timed out"),
                "the refusal must name the acquire timeout so an operator can see the \
         ceiling was hit; got {message}"
            );

            drop(first);

            // And the ceiling is a queue, not a wall: once the lease returns, the next
            // checkout succeeds.
            let third = backend
                .fixture_session("app_pool_probe")
                .await
                .expect("checkout after the first lease returned");
            drop(third);
        })
    })
}
