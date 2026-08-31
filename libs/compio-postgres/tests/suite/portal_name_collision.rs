//! Ownership coverage for a Bind rejected on a generated portal-name collision.

use std::fmt::Write;

use compio_postgres::error::SqlState;
use compio_postgres::{Client, SimpleQueryMessage};

#[allow(unused_imports)]
use crate::common;

async fn connected() -> Client {
    let url = common::test_url();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

async fn portal_name_for_marker(
    transaction: &compio_postgres::Transaction<'_>,
    marker: &str,
) -> String {
    let messages = transaction
        .simple_query(&format!(
            "SELECT name FROM pg_cursors \
             WHERE name LIKE 'p%' AND statement LIKE '%{marker}%'"
        ))
        .await
        .expect("inspect the live portal name without advancing the Bind counter");
    let names = messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 1, "expected one marked portal, got {names:?}");
    names[0].to_string()
}

async fn cursor_count(transaction: &compio_postgres::Transaction<'_>, name: &str) -> i64 {
    transaction
        .query_one(
            "SELECT count(*)::int8 FROM pg_cursors WHERE name = $1",
            &[&name],
        )
        .await
        .expect("inspect the replacement cursor")
        .get(0)
}

fn portal_id(name: &str) -> usize {
    name.strip_prefix('p')
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("driver portal name {name:?} is not p followed by a usize"))
}

/// Reserve enough consecutive names that other tests cannot advance bind.rs's
/// process-global counter past every collision between the probe and the Bind.
async fn reserve_portal_names(
    transaction: &compio_postgres::Transaction<'_>,
    after: usize,
) -> usize {
    const RESERVATIONS: usize = 4096;
    let mut sql = String::with_capacity(RESERVATIONS * 80);
    for offset in 1..=RESERVATIONS {
        let id = after.wrapping_add(offset);
        writeln!(
            &mut sql,
            "DECLARE p{id} NO SCROLL CURSOR WITHOUT HOLD \
             FOR SELECT 99::int4 /* cpg_reserved_portal_owner */;"
        )
        .unwrap();
    }
    transaction
        .batch_execute(&sql)
        .await
        .expect("reserve generated-looking portal names");
    RESERVATIONS
}

#[compio::test]
async fn rejected_bind_does_not_close_the_portal_that_owned_the_name() {
    const MARKER: &str = "cpg_portal_name_probe";

    let mut client = connected().await;
    let mut transaction = client.transaction().await.expect("begin outer transaction");
    let statement = transaction
        .prepare("SELECT 1::int4 /* cpg_portal_name_collision */")
        .await
        .expect("prepare the statement whose Bind will collide");

    let probe = transaction
        .bind("SELECT 0::int4 /* cpg_portal_name_probe */", &[])
        .await
        .expect("bind a probe portal");
    let probe_name = portal_name_for_marker(&transaction, MARKER).await;
    let probe_id = portal_id(&probe_name);
    drop(probe);
    transaction
        .simple_query("")
        .await
        .expect("wait for the probe's Close(P)");

    reserve_portal_names(&transaction, probe_id).await;

    let nested = transaction
        .transaction()
        .await
        .expect("isolate the expected duplicate-cursor error in a savepoint");
    let error = match nested.bind(&statement, &[]).await {
        Err(error) => error,
        Ok(_) => panic!("a reserved portal name must reject Bind"),
    };
    assert_eq!(error.code(), Some(&SqlState::DUPLICATE_CURSOR));
    let collided_name = error
        .as_db_error()
        .and_then(|error| error.message().split('"').nth(1))
        .expect("duplicate-cursor error names the colliding portal")
        .to_string();
    nested
        .rollback()
        .await
        .expect("recover the outer transaction after the rejected Bind");

    let fetched = transaction
        .simple_query(&format!("FETCH ALL FROM {collided_name}"))
        .await
        .expect("rejected Bind must not close the cursor which already owned its name");
    let value = fetched.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => row.get(0),
        _ => None,
    });
    assert_eq!(value, Some("99"));

    transaction
        .rollback()
        .await
        .expect("roll back outer transaction");
}

#[compio::test]
async fn cancelled_rejected_bind_does_not_close_the_portal_that_owned_the_name() {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    const MARKER: &str = "cpg_cancelled_portal_name_probe";

    let mut client = connected().await;
    let mut transaction = client.transaction().await.expect("begin outer transaction");
    let statement = transaction
        .prepare("SELECT 2::int4 /* cpg_cancelled_portal_name_collision */")
        .await
        .expect("prepare the statement whose abandoned Bind will collide");

    let probe = transaction
        .bind("SELECT 0::int4 /* cpg_cancelled_portal_name_probe */", &[])
        .await
        .expect("bind a probe portal");
    let probe_name = portal_name_for_marker(&transaction, MARKER).await;
    let probe_id = portal_id(&probe_name);
    drop(probe);
    transaction
        .simple_query("")
        .await
        .expect("wait for the probe's Close(P)");

    let reserved = reserve_portal_names(&transaction, probe_id).await;
    let nested = transaction
        .transaction()
        .await
        .expect("isolate the expected duplicate-cursor error in a savepoint");
    {
        let mut bind = pin!(nested.bind(&statement, &[]));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            matches!(bind.as_mut().poll(&mut context), Poll::Pending),
            "the first poll must only enqueue Bind, not consume its response"
        );
    }
    nested
        .rollback()
        .await
        .expect("recover the outer transaction after abandoning rejected Bind");

    let remaining: i64 = transaction
        .query_one(
            "SELECT count(*)::int8 FROM pg_cursors \
             WHERE name LIKE 'p%' \
             AND statement LIKE '%cpg_reserved_portal_owner%'",
            &[],
        )
        .await
        .expect("count the portals which owned the reserved names")
        .get(0);
    transaction
        .rollback()
        .await
        .expect("roll back outer transaction");

    assert_eq!(
        remaining, reserved as i64,
        "abandoning rejected Bind closed a pre-existing portal"
    );
}

#[compio::test]
async fn expired_portal_cannot_execute_a_replacement_with_the_same_name() {
    const MARKER: &str = "cpg_expired_portal_execute";

    let mut client = connected().await;
    let transaction = client
        .transaction()
        .await
        .expect("begin original transaction");
    let stale = transaction
        .bind(
            "SELECT 'original'::text /* cpg_expired_portal_execute */",
            &[],
        )
        .await
        .expect("bind the original portal");
    let name = portal_name_for_marker(&transaction, MARKER).await;
    transaction
        .commit()
        .await
        .expect("commit drops the server-side original portal");

    let transaction = client
        .transaction()
        .await
        .expect("begin replacement transaction");
    transaction
        .batch_execute(&format!(
            "DECLARE {name} NO SCROLL CURSOR WITHOUT HOLD \
             FOR SELECT 'replacement'::text"
        ))
        .await
        .expect("create a replacement portal with the expired handle's name");

    let outcome = transaction.query_portal(&stale, 0).await.map(|rows| {
        rows.iter()
            .map(|row| row.try_get::<_, String>(0))
            .collect::<Vec<_>>()
    });
    transaction
        .rollback()
        .await
        .expect("roll back replacement transaction");

    assert!(
        outcome.is_err(),
        "an expired portal executed an unrelated replacement: {outcome:?}"
    );
}

#[compio::test]
async fn dropping_an_expired_portal_does_not_close_its_replacement() {
    const MARKER: &str = "cpg_expired_portal_drop";

    let mut client = connected().await;
    let transaction = client
        .transaction()
        .await
        .expect("begin original transaction");
    let stale = transaction
        .bind("SELECT 1::int4 /* cpg_expired_portal_drop */", &[])
        .await
        .expect("bind the original portal");
    let name = portal_name_for_marker(&transaction, MARKER).await;
    transaction
        .commit()
        .await
        .expect("commit drops the server-side original portal");

    let transaction = client
        .transaction()
        .await
        .expect("begin replacement transaction");
    transaction
        .batch_execute(&format!(
            "DECLARE {name} NO SCROLL CURSOR WITHOUT HOLD FOR SELECT 99::int4"
        ))
        .await
        .expect("create a replacement portal with the expired handle's name");
    let before = cursor_count(&transaction, &name).await;

    drop(stale);
    transaction
        .simple_query("")
        .await
        .expect("wait for the expired handle's deferred Close(P)");
    let after = cursor_count(&transaction, &name).await;
    let fetched = transaction
        .simple_query(&format!("FETCH ALL FROM {name}"))
        .await;
    transaction
        .rollback()
        .await
        .expect("roll back replacement transaction");

    assert_eq!(before, 1, "the replacement cursor was not created");
    assert_eq!(
        after, 1,
        "dropping the expired Rust handle closed the replacement cursor"
    );
    let fetched = fetched.expect("the replacement cursor must remain fetchable");
    let value = fetched.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => row.get(0),
        _ => None,
    });
    assert_eq!(value, Some("99"));
}

#[compio::test]
async fn portal_from_committed_savepoint_remains_live_in_parent() {
    let mut client = connected().await;
    let mut outer = client.transaction().await.expect("begin outer transaction");
    let inner = outer.transaction().await.expect("create savepoint");
    let portal = inner
        .bind("SELECT 41::int4, 42::int4", &[])
        .await
        .expect("bind a portal in the savepoint");
    inner
        .commit()
        .await
        .expect("release reparents the server portal");

    let row = outer
        .query_portal(&portal, 0)
        .await
        .expect("a portal survives its creating savepoint's release")
        .pop()
        .expect("portal returned its row");
    assert_eq!(row.get::<_, i32>(0), 41);
    assert_eq!(row.get::<_, i32>(1), 42);
    outer.rollback().await.expect("roll back outer transaction");
}

#[compio::test]
async fn rolled_back_savepoint_portal_cannot_touch_a_reused_name() {
    const MARKER: &str = "cpg_rolled_back_savepoint_portal";

    let mut client = connected().await;
    let mut outer = client.transaction().await.expect("begin outer transaction");
    let inner = outer.transaction().await.expect("create savepoint");
    let stale = inner
        .bind(
            "SELECT 'savepoint'::text /* cpg_rolled_back_savepoint_portal */",
            &[],
        )
        .await
        .expect("bind a portal in the savepoint");
    let name = portal_name_for_marker(&inner, MARKER).await;
    inner
        .rollback()
        .await
        .expect("rollback drops the savepoint's server portal");

    outer
        .batch_execute(&format!(
            "DECLARE {name} NO SCROLL CURSOR WITHOUT HOLD FOR SELECT 73::int4"
        ))
        .await
        .expect("reuse the rolled-back portal's name in the parent");
    assert!(
        outer.query_portal(&stale, 0).await.is_err(),
        "a rolled-back savepoint portal executed its replacement"
    );
    drop(stale);
    outer
        .simple_query("")
        .await
        .expect("flush any deferred portal cleanup");
    assert_eq!(cursor_count(&outer, &name).await, 1);

    let fetched = outer
        .simple_query(&format!("FETCH ALL FROM {name}"))
        .await
        .expect("the stale handle must not close the replacement");
    let value = fetched.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => row.get(0),
        _ => None,
    });
    assert_eq!(value, Some("73"));
    outer.rollback().await.expect("roll back outer transaction");
}

#[compio::test]
async fn parent_rollback_invalidates_a_portal_reparented_from_a_grandchild() {
    let mut client = connected().await;
    let mut outer = client.transaction().await.expect("begin outer transaction");
    let mut child = outer.transaction().await.expect("create child savepoint");
    let grandchild = child
        .transaction()
        .await
        .expect("create grandchild savepoint");
    let stale = grandchild
        .bind("SELECT 86::int4", &[])
        .await
        .expect("bind a portal in the grandchild");
    grandchild
        .commit()
        .await
        .expect("release reparents the portal to the child");
    child
        .rollback()
        .await
        .expect("rolling back the child drops the reparented portal");

    assert!(
        outer.query_portal(&stale, 0).await.is_err(),
        "a child rollback left its reparented grandchild portal live"
    );
    outer.rollback().await.expect("roll back outer transaction");
}
