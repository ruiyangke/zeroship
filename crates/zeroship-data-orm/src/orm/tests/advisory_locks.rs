#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;
use futures::{
    channel::oneshot,
    future::{select, Either},
    FutureExt,
};
use std::time::Duration;

schema! {
    pub advisory_schema {
        advisory_rows {
            #[orm(primary_key)]
            id: Text,
            label: Text,
        }
    }
}
use advisory_schema::advisory_rows;

#[derive(Debug, FromRow)]
#[orm(entity = advisory_rows)]
struct AdvisoryRow {
    id: String,
    label: String,
}

async fn fixture(postgres: bool, pool_size: usize) -> CollectionFixture {
    let fields = value!({
        "id": {"type":"string", "primaryKey":true, "required":true},
        "label": {"type":"string", "required":true}
    });
    let columns = "id TEXT PRIMARY KEY, label TEXT NOT NULL";
    let mut owner = if postgres {
        CollectionFixture::postgres_from_table_definition_with_pool_size(
            "advisory_rows",
            fields,
            columns,
            pool_size,
        )
        .await
    } else {
        CollectionFixture::sqlite_from_table_definition("advisory_rows", fields, columns).await
    };
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        advisory_schema::schema(),
    )
    .unwrap();
    owner
}

fn assert_code(error: DbError, expected: &str) {
    let matched =
        matches!(&error, DbError::ValidationFailed { code, .. } if *code == expected);
    assert!(matched, "expected {expected}: {}", error.into_string());
}

fn postgres_backend(owner: &CollectionFixture) -> &crate::backend::postgres::PostgresBackend {
    owner
        .database
        .backend
        .get::<crate::backend::postgres::PostgresBackend>()
        .unwrap()
}

fn qualified(owner: &CollectionFixture) -> String {
    format!(
        "{}.advisory_rows",
        crate::sql::mapping::quote_ident(owner.database.binding.schema().as_str())
    )
}

/// A connection outside the ORM's pool, used as the lock oracle.
async fn oracle(owner: &CollectionFixture) -> compio_postgres::Pool {
    compio_postgres::Pool::connect(postgres_backend(owner).url(), 2)
        .await
        .unwrap()
}

/// Try the key in its own implicit transaction, spelled the way raw callers
/// spell each form. An acquired key is released when the statement ends.
async fn free(oracle: &compio_postgres::Client, key: &AdvisoryKey) -> bool {
    let row = match key {
        AdvisoryKey::Single(key) => {
            oracle
                .query_one("SELECT pg_try_advisory_xact_lock($1::int8)", &[key])
                .await
        }
        AdvisoryKey::Pair(high, low) => {
            oracle
                .query_one(
                    "SELECT pg_try_advisory_xact_lock($1::int4, $2::int4)",
                    &[high, low],
                )
                .await
        }
        AdvisoryKey::HashedPair { namespace, text } => {
            oracle
                .query_one(
                    "SELECT pg_try_advisory_xact_lock($1::int4, hashtext($2::text))",
                    &[namespace, text],
                )
                .await
        }
        AdvisoryKey::Hashed(text) => {
            oracle
                .query_one(
                    "SELECT pg_try_advisory_xact_lock(hashtext($1::text)::bigint)",
                    &[text],
                )
                .await
        }
        AdvisoryKey::HashedLowercase(text) => {
            oracle
                .query_one(
                    "SELECT pg_try_advisory_xact_lock(hashtext(lower($1::text))::bigint)",
                    &[text],
                )
                .await
        }
    };
    row.unwrap().get(0)
}

async fn eventually_free(oracle: &compio_postgres::Client, key: &AdvisoryKey) {
    compio::time::timeout(Duration::from_secs(5), async {
        while !free(oracle, key).await {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the advisory lock must be released");
}

/// Wait for another session to wait on an advisory lock and return its pid.
async fn advisory_waiter(oracle: &compio_postgres::Client) -> i32 {
    compio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(row) = oracle
                .query_opt(
                    "SELECT pid FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
                     AND wait_event = 'advisory' LIMIT 1",
                    &[],
                )
                .await
                .unwrap()
            {
                return row.get(0);
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the ORM transaction must reach its advisory lock wait")
}

#[compio::test]
async fn postgres_advisory_xact_lock_contends_with_its_raw_oracle_for_every_key_form() {
    #[derive(Clone, Copy, Debug)]
    enum Settlement {
        Commit,
        Rollback,
        Cancel,
    }
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let oracle = oracle.acquire().await.unwrap();
    let forms = [
        (
            AdvisoryKey::single(0x7a55_0001_0000_0001),
            AdvisoryKey::single(0x7a55_0001_0000_0002),
        ),
        (AdvisoryKey::pair(0x7a55_0001, 7), AdvisoryKey::pair(0x7a55_0001, 8)),
        (
            AdvisoryKey::hashed_pair(0x7a55_0001, "usr_first"),
            AdvisoryKey::hashed_pair(0x7a55_0002, "usr_first"),
        ),
        (
            AdvisoryKey::hashed("family-a"),
            AdvisoryKey::hashed("family-b"),
        ),
        (
            AdvisoryKey::hashed_lowercase("Person@Example.test"),
            AdvisoryKey::hashed_lowercase("Person@Example.tesu"),
        ),
    ];
    for (key, distinct) in forms {
        for settlement in [Settlement::Commit, Settlement::Rollback, Settlement::Cancel] {
            let (held, ready) = oneshot::channel();
            let (release, resume) = oneshot::channel::<()>();
            let acquire = key.clone();
            let holding = owner
                .database
                .transaction(|tx| async move {
                    tx.postgres()?.advisory_xact_lock(acquire).await?;
                    held.send(()).unwrap();
                    let _ = resume.await;
                    if matches!(settlement, Settlement::Rollback) {
                        Err(DbError::validation("test_rollback", "release the lock"))
                    } else {
                        Ok(())
                    }
                })
                .boxed_local();
            let holding = match select(ready, holding).await {
                Either::Left((ready, holding)) => {
                    ready.unwrap();
                    holding
                }
                Either::Right((result, _)) => panic!("lock callback ended early: {result:?}"),
            };
            assert!(!free(&oracle, &key).await, "{key:?} must contend while held");
            assert!(
                free(&oracle, &distinct).await,
                "{distinct:?} must stay independent of {key:?}"
            );
            if matches!(settlement, Settlement::Cancel) {
                drop(holding);
                drop(release);
                eventually_free(&oracle, &key).await;
            } else {
                release.send(()).unwrap();
                let settled = holding.await;
                assert_eq!(
                    settled.is_err(),
                    matches!(settlement, Settlement::Rollback),
                    "{settlement:?}: {settled:?}"
                );
                assert!(free(&oracle, &key).await, "{key:?} after {settlement:?}");
            }
        }
    }
    drop(oracle);
    owner.close().await;
}

#[compio::test]
async fn postgres_advisory_key_spaces_and_case_folding() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let oracle = oracle.acquire().await.unwrap();
    let oracle = &oracle;
    owner
        .database
        .transaction(|tx| async move {
            let postgres = tx.postgres()?;
            let bits = (0x42_i64 << 32) | 7;
            postgres.advisory_xact_lock(AdvisoryKey::single(bits)).await?;
            assert!(!free(oracle, &AdvisoryKey::single(bits)).await);
            assert!(
                free(oracle, &AdvisoryKey::pair(0x42, 7)).await,
                "a pair with the same bits is a separate key space"
            );

            postgres
                .advisory_xact_lock(AdvisoryKey::hashed_pair(0x43, "subject"))
                .await?;
            let widened: i64 = oracle
                .query_one(
                    "SELECT ($1::bigint << 32) | (hashtext($2::text)::bigint & 4294967295)",
                    &[&0x43_i64, &"subject"],
                )
                .await
                .unwrap()
                .get(0);
            assert!(!free(oracle, &AdvisoryKey::hashed_pair(0x43, "subject")).await);
            assert!(
                free(oracle, &AdvisoryKey::single(widened)).await,
                "a hashed pair does not hold the single key with the same bits"
            );

            postgres
                .advisory_xact_lock(AdvisoryKey::hashed_lowercase("MiXeD@x.test"))
                .await?;
            assert!(
                !free(oracle, &AdvisoryKey::hashed("mixed@x.test")).await,
                "the database folds case before hashing"
            );
            // Control: hashing without folding names a different key.
            assert!(free(oracle, &AdvisoryKey::hashed("MiXeD@x.test")).await);
            Ok(())
        })
        .await
        .unwrap();
    owner.close().await;
}

#[compio::test]
async fn postgres_advisory_xact_lock_stacks_and_releases_once_at_settlement() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let oracle = oracle.acquire().await.unwrap();
    let oracle = &oracle;
    let key = AdvisoryKey::hashed("stacked");
    let inside = key.clone();
    owner
        .database
        .transaction(|tx| async move {
            let postgres = tx.postgres()?;
            postgres.advisory_xact_lock(inside.clone()).await?;
            postgres.advisory_xact_lock(inside.clone()).await?;
            let holds: i64 = oracle
                .query_one(
                    "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted \
                     AND pid <> pg_backend_pid()",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(holds, 1, "a stacked key is one lock");
            assert!(!free(oracle, &inside).await);
            Ok(())
        })
        .await
        .unwrap();
    assert!(free(oracle, &key).await, "settlement releases every stacked hold");
    owner.close().await;
}

#[compio::test]
async fn postgres_waiter_sees_the_holders_commit_in_its_next_statement_under_read_committed() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let holder = oracle.acquire().await.unwrap();
    let observer = oracle.acquire().await.unwrap();
    let table = qualified(&owner);
    for (level, id, visible) in [
        (IsolationLevel::ReadCommitted, "read_committed", true),
        (IsolationLevel::RepeatableRead, "repeatable_read", false),
    ] {
        holder
            .batch_execute("BEGIN; SELECT pg_advisory_xact_lock(4242)")
            .await
            .unwrap();
        let waiting = owner
            .database
            .transaction_with_options(
                TransactionOptions::default().isolation_level(level),
                |tx| async move {
                    tx.postgres()?
                        .advisory_xact_lock(AdvisoryKey::single(4242))
                        .await?;
                    tx.entity::<advisory_rows::Entity>()?
                        .query()
                        .filter(advisory_rows::id.eq(id)?)
                        .first::<AdvisoryRow>()
                        .await
                },
            )
            .boxed_local();
        let waiting = match select(advisory_waiter(&observer).boxed_local(), waiting).await {
            Either::Left((_, waiting)) => waiting,
            Either::Right((result, _)) => panic!("the ORM lock did not wait: {result:?}"),
        };
        holder
            .batch_execute(&format!(
                "INSERT INTO {table} VALUES ('{id}', 'holder'); COMMIT"
            ))
            .await
            .unwrap();
        let row = compio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the waiter must resume after the holder commits")
            .unwrap();
        assert_eq!(row.is_some(), visible, "{level:?}");
        if let Some(row) = row {
            assert_eq!((row.id.as_str(), row.label.as_str()), (id, "holder"));
        }
    }
    drop(observer);
    drop(holder);
    owner.close().await;
}

#[compio::test]
async fn postgres_advisory_wait_is_bounded_by_the_transaction_lock_budget() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let holder = oracle.acquire().await.unwrap();
    holder
        .batch_execute("SELECT pg_advisory_lock(5151)")
        .await
        .unwrap();
    let error = owner
        .database
        .transaction(|tx| async move {
            tx.collection("advisory_rows")?
                .insert(value!({"id":"before-timeout", "label":"rolled back"}))
                .await?;
            tx.postgres()?
                .advisory_xact_lock(AdvisoryKey::single(5151))
                .await
        })
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::LockContention { .. }), "{error:?}");
    let rows = owner
        .database
        .entity::<advisory_rows::Entity>()
        .unwrap()
        .query()
        .all::<AdvisoryRow>()
        .await
        .unwrap();
    assert!(rows.is_empty(), "the timed-out transaction must roll back: {rows:?}");
    // Control: once the holder releases, the same lock is acquired.
    holder
        .batch_execute("SELECT pg_advisory_unlock(5151)")
        .await
        .unwrap();
    owner
        .database
        .transaction(|tx| async move {
            tx.postgres()?
                .advisory_xact_lock(AdvisoryKey::single(5151))
                .await
        })
        .await
        .unwrap();
    drop(holder);
    owner.close().await;
}

#[compio::test]
async fn advisory_xact_lock_requires_the_transaction_receiver() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let oracle = oracle.acquire().await.unwrap();
    let oracle = &oracle;
    let key = AdvisoryKey::single(6161);
    assert_code(
        owner
            .database
            .postgres()
            .unwrap()
            .advisory_xact_lock(key.clone())
            .await
            .unwrap_err(),
        "transaction_required",
    );
    assert!(free(oracle, &key).await, "a refused request takes no lock");
    let root = owner.database.clone();
    let inside = key.clone();
    owner
        .database
        .transaction(|tx| async move {
            // A root handle stays a pooled receiver inside another callback.
            assert_code(
                root.postgres()?
                    .advisory_xact_lock(inside.clone())
                    .await
                    .unwrap_err(),
                "transaction_required",
            );
            // Control: the transaction handle takes the lock.
            tx.postgres()?.advisory_xact_lock(inside.clone()).await?;
            assert!(!free(oracle, &inside).await);
            Ok(())
        })
        .await
        .unwrap();
    owner.close().await;
}

#[compio::test]
async fn sqlite_refuses_postgres_extension() {
    let owner = fixture(false, 4).await;
    assert_code(
        owner.database.postgres().unwrap_err(),
        "unsupported_backend_feature",
    );
    owner
        .database
        .transaction(|tx| async move {
            assert_code(tx.postgres().unwrap_err(), "unsupported_backend_feature");
            // Control: the transaction continues and commits.
            tx.collection("advisory_rows")?
                .insert(value!({"id":"kept", "label":"committed"}))
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let rows = owner
        .database
        .entity::<advisory_rows::Entity>()
        .unwrap()
        .query()
        .all::<AdvisoryRow>()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    owner.close().await;
}

#[compio::test]
async fn postgres_advisory_lock_in_rolled_back_savepoint() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let oracle = oracle.acquire().await.unwrap();
    let oracle = &oracle;
    owner
        .database
        .transaction(|tx| async move {
            let root_key = AdvisoryKey::single(7171);
            let nested_key = AdvisoryKey::single(7172);
            tx.postgres()?.advisory_xact_lock(root_key.clone()).await?;
            let inner_key = nested_key.clone();
            let nested = tx
                .transaction(|inner| async move {
                    inner.postgres()?.advisory_xact_lock(inner_key.clone()).await?;
                    assert!(!free(oracle, &inner_key).await);
                    Err::<(), _>(DbError::validation("test_rollback", "roll back the frame"))
                })
                .await;
            assert_code(nested.unwrap_err(), "test_rollback");
            // Observed contract: rolling back the savepoint released its lock.
            assert!(free(oracle, &nested_key).await);
            // Control: the root frame's lock survives the nested rollback.
            assert!(!free(oracle, &root_key).await);
            Ok(())
        })
        .await
        .unwrap();
    owner.close().await;
}

#[compio::test]
async fn postgres_cancelled_advisory_wait_leaves_a_clean_pool() {
    let owner = fixture(true, 1).await;
    let oracle = oracle(&owner).await;
    let holder = oracle.acquire().await.unwrap();
    let observer = oracle.acquire().await.unwrap();
    holder
        .batch_execute("SELECT pg_advisory_lock(8181)")
        .await
        .unwrap();
    let waiting = owner
        .database
        .transaction(|tx| async move {
            tx.postgres()?
                .advisory_xact_lock(AdvisoryKey::single(8181))
                .await
        })
        .boxed_local();
    let (pid, waiting) = match select(advisory_waiter(&observer).boxed_local(), waiting).await {
        Either::Left((pid, waiting)) => (pid, waiting),
        Either::Right((result, _)) => panic!("the ORM lock did not wait: {result:?}"),
    };
    drop(waiting);
    // With a pool of one, the next transaction needs the cancelled session's
    // capacity back.
    compio::time::timeout(
        Duration::from_secs(5),
        owner.database.transaction(|tx| async move {
            tx.collection("advisory_rows")?
                .insert(value!({"id":"after-cancel", "label":"committed"}))
                .await?;
            Ok(())
        }),
    )
    .await
    .expect("a cancelled wait must release its pooled session")
    .unwrap();
    let leftovers: i64 = observer
        .query_one(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND pid = $1",
            &[&pid],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(leftovers, 0, "the cancelled session holds no advisory lock");
    // Control: the holder's session lock was never disturbed.
    assert!(!free(&observer, &AdvisoryKey::single(8181)).await);
    holder
        .batch_execute("SELECT pg_advisory_unlock(8181)")
        .await
        .unwrap();
    drop(observer);
    drop(holder);
    owner.close().await;
}
