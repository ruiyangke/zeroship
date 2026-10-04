//! The `PostgreSQL` fixture now clones its database from one bare server shared
//! by the whole run, rather than booting a container per case
//! (`crates/zeroship-testkit/src/postgres/server.rs`). These contracts pin two
//! things a case must keep true under that sharing: the server really is
//! shared, and a case still cannot see another case's role or rows on it.
#![allow(
    clippy::future_not_send,
    reason = "native fixtures stay on their compio runtime"
)]

use crate::support::{self, Admin, Backend, Fixture};
use zeroship_core::app_id::AppId;
use zeroship_workflow_manager::{Options, Queue};

/// Before the shared bare server, each `Fixture::new(Backend::Postgres)`
/// booted its own `testcontainers` container, so this equality did not hold;
/// it is the one assertion this file makes that the earlier fixture could not
/// have passed.
#[compio::test]
async fn two_cases_share_one_container() {
    let first = Fixture::new(Backend::Postgres).await;
    let second = Fixture::new(Backend::Postgres).await;
    assert_eq!(
        first.container_id(),
        second.container_id(),
        "two cases of one run must clone from the same bare server container"
    );
    assert_ne!(
        first.database_name(),
        second.database_name(),
        "each case must still own a database of its own"
    );
    assert_ne!(
        first.role(),
        second.role(),
        "each case must mint a role of its own, not a fixed shared name"
    );
}

/// A row a case writes lives in its own clone, not the shared server: another
/// case's admin connection, pointed at the same table name, reads nothing.
#[compio::test]
async fn a_case_cannot_see_another_cases_rows() {
    let first = Fixture::new(Backend::Postgres).await;
    let second = Fixture::new(Backend::Postgres).await;

    let queue = Queue::connect(
        first.binding(),
        first.url(),
        Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let app = AppId::mint();
    queue
        .register_scope(&app, &zeroship_core::ZoneId::default_zone())
        .await
        .unwrap();

    let Admin::Postgres(first_admin) = &first.admin else {
        unreachable!()
    };
    let Admin::Postgres(second_admin) = &second.admin else {
        unreachable!()
    };
    let present = first_admin
        .query(
            "SELECT 1 FROM workflow_manager.queue_scopes WHERE id = $1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(present.len(), 1, "the writing case must read its own row back");
    let absent = second_admin
        .query(
            "SELECT 1 FROM workflow_manager.queue_scopes WHERE id = $1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert!(
        absent.is_empty(),
        "a sibling case's clone must not carry this case's row"
    );
}

/// A role is a cluster-global object - it authenticates on any database of the
/// shared server - but it was never granted `CONNECT` on a sibling case's
/// clone, so authorization, not authentication, is what keeps cases apart.
#[compio::test]
async fn a_cases_role_cannot_connect_to_another_cases_database() {
    let first = Fixture::new(Backend::Postgres).await;
    let second = Fixture::new(Backend::Postgres).await;

    let crossed_url = second.role_url(first.role());
    let crossed = compio_postgres::connect(&crossed_url, compio_postgres::NoTls).await;
    let error = crossed.expect_err(
        "the first case's role must not reach the second case's database, \
         even though the role itself exists cluster-wide",
    );
    let db_error = error
        .as_db_error()
        .expect("the server must answer with a SQLSTATE, not a transport failure");
    assert_eq!(
        db_error.code().code(),
        "42501",
        "the refusal must be insufficient_privilege, not an authentication failure: {error}"
    );

    // Control: the same role reaches its own database without trouble, so the
    // refusal above is the cross-case grant and not a malformed URL or a
    // wrong password.
    let own_url = first.role_url(first.role());
    let (client, connection) = compio_postgres::connect(&own_url, compio_postgres::NoTls)
        .await
        .expect("the first case's own role must connect to its own database");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
        .query("SELECT 1", &[])
        .await
        .expect("the first case's role can at least read on its own database");
}

/// `support::retention::Catalog` mints its own login on the fixture's
/// database (`tests/support/retention.rs`); nothing else revokes or drops it,
/// so its own `Drop` must. `pg_roles` is cluster-wide, so this checks the
/// whole shared server, not just this case's clone.
#[compio::test]
async fn a_catalogs_role_is_gone_once_the_catalog_drops() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let role = {
        let catalog = support::retention::Catalog::new(&fixture).await;
        catalog.role().to_owned()
    };
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    let remaining = admin
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .unwrap();
    assert!(
        remaining.is_empty(),
        "the catalog's role \"{role}\" must not survive the Catalog that minted it"
    );
}
