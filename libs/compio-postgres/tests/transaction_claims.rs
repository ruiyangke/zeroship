//! Mutation-sensitive coverage for runtime guarantees documented by
//! `Transaction`.

use compio_postgres::{Client, Error, NoTls};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    Ok(client)
}

/// `Transaction::bind` reports a parameter-arity mistake the same way its
/// siblings do: as an `Err`, not a panic, and not a `Portal` bound to the
/// wrong shape.
///
/// The doc comment on `bind` claimed a panic until 2026-08-21 and the code
/// never panicked. Making the code match the comment was the wrong direction:
/// `query`, `execute` and the `_raw` forms all return `Error::parameters` for
/// the identical mistake, so panicking here would have made the outcome depend
/// on which method the caller reached for. The comment was corrected instead,
/// and this test is what keeps the two from drifting apart again.
///
/// `catch_unwind` is deliberate. Asserting only on the `Err` would still pass
/// if someone reintroduced the panic, because the panic would abort the test
/// before the assertion ran and `#[compio::test]` would report the failure as
/// a panic rather than as this claim being violated.
#[compio::test]
async fn bind_reports_parameter_count_mismatch_as_an_error() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let transaction = client.transaction().await.unwrap();
        let statement = transaction.prepare("SELECT $1::int4").await.unwrap();

        let outcome = AssertUnwindSafe(transaction.bind(&statement, &[]))
            .catch_unwind()
            .await;

        let complaint = match outcome {
            Err(_) => Some("panicked".to_string()),
            Ok(Ok(_)) => Some("returned Ok(Portal) for a statement expecting 1 parameter".to_string()),
            Ok(Err(error)) => {
                // The counts both appear, and in the right roles: one
                // parameter expected, none supplied.
                let chain = common::error_chain(&error);
                if chain.contains("expected 1 parameters but got 0") {
                    None
                } else {
                    Some(format!("returned Err({chain}), which does not name both counts"))
                }
            }
        };

        // The failed bind must not have left the transaction unusable.
        let still_alive: i32 = transaction
            .query_one("SELECT 7::int4", &[])
            .await
            .expect("the transaction stopped working after a rejected bind")
            .get(0);
        assert_eq!(still_alive, 7);
        transaction.rollback().await.unwrap();

        if let Some(complaint) = complaint {
            panic!("Transaction::bind {complaint}; it must report arity mistakes as an Err");
        }
    })
    .await
    .expect("bind arity claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn successful_commit_clears_dirty_after_nested_savepoint_drop() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        let mut transaction = client.transaction().await.unwrap();
        let nested = transaction.transaction().await.unwrap();
        drop(nested);
        assert!(
            transaction.client().is_dirty(),
            "dropping the nested savepoint did not mark its client dirty"
        );

        transaction.commit().await.unwrap();
        assert!(
            !client.is_dirty(),
            "a successful commit left the nested rollback's dirty flag set"
        );
    })
    .await
    .expect("commit dirty-state claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn successful_rollback_clears_dirty_after_nested_savepoint_drop() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        let mut transaction = client.transaction().await.unwrap();
        let nested = transaction.transaction().await.unwrap();
        drop(nested);
        assert!(
            transaction.client().is_dirty(),
            "dropping the nested savepoint did not mark its client dirty"
        );

        transaction.rollback().await.unwrap();
        assert!(
            !client.is_dirty(),
            "a successful rollback left the nested rollback's dirty flag set"
        );
    })
    .await
    .expect("rollback dirty-state claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn nonpositive_portal_limits_return_every_row() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let transaction = client.transaction().await.unwrap();
        let statement = transaction
            .prepare("SELECT i::int4 FROM generate_series(1, 5) AS i ORDER BY i")
            .await
            .unwrap();

        for max_rows in [0, -1] {
            let portal = transaction.bind(&statement, &[]).await.unwrap();
            let rows = transaction.query_portal(&portal, max_rows).await.unwrap();
            let values = rows
                .iter()
                .map(|row| row.get::<_, i32>(0))
                .collect::<Vec<_>>();
            assert_eq!(
                values,
                [1, 2, 3, 4, 5],
                "query_portal({max_rows}) did not return every row"
            );
        }

        transaction.rollback().await.unwrap();
    })
    .await
    .expect("portal row-limit claim test exceeded its 10 second deadline");
}
