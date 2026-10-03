//! The workflow journal fixture's shared-server contracts.
//!
//! Two cases of a run share one bare server and still work in databases of
//! their own, and that server admits the connection fan-out the suite's pools
//! and case databases produce.

use super::*;

/// Two cases share one server and still work in databases of their own: a
/// table one creates is not visible to the other.
#[compio::test]
async fn cases_share_the_server_and_keep_separate_databases() {
    let first = PostgresFixture::start().await;
    let second = PostgresFixture::start().await;
    assert_eq!(
        first.container_id(),
        second.container_id(),
        "cases must share the worktree's server"
    );
    assert_ne!(
        first.admin_url, second.admin_url,
        "each case must work in its own database"
    );
    let one = connect(&first.admin_url).await;
    one.batch_execute("CREATE TABLE isolation_probe (id text)")
        .await
        .unwrap();
    let two = connect(&second.admin_url).await;
    assert!(
        two.batch_execute("SELECT * FROM isolation_probe")
            .await
            .is_err(),
        "a case database must not see another case's tables"
    );
}

/// The shared server admits more connections than PostgreSQL's default, so the
/// case databases and their connection pools of a run all fit.
#[compio::test]
async fn the_shared_server_admits_the_suite_connection_fan_out() {
    let fixture = PostgresFixture::start().await;
    let mut clients = Vec::new();
    for _ in 0..128 {
        clients.push(connect(&fixture.admin_url).await);
    }
    for client in &clients {
        client.batch_execute("SELECT 1").await.unwrap();
    }
}
