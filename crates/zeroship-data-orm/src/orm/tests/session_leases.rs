#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;
use std::time::Duration;

schema! {
    pub lease_schema {
        lease_rows {
            #[orm(primary_key)]
            id: Text,
            label: Text,
        }
    }
}

const KEY: i64 = 0x0000_1234_0000_5678;

async fn fixture(postgres: bool, pool_size: usize) -> CollectionFixture {
    let fields = value!({
        "id": {"type":"string", "primaryKey":true, "required":true},
        "label": {"type":"string", "required":true}
    });
    let columns = "id TEXT PRIMARY KEY, label TEXT NOT NULL";
    let mut owner = if postgres {
        CollectionFixture::postgres_from_table_definition_with_pool_size(
            "lease_rows",
            fields,
            columns,
            pool_size,
        )
        .await
    } else {
        CollectionFixture::sqlite_from_table_definition("lease_rows", fields, columns).await
    };
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        lease_schema::schema(),
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

/// Connections outside the ORM's pool, used as lock oracles.
async fn oracle(owner: &CollectionFixture) -> compio_postgres::PoolConnection {
    compio_postgres::Pool::connect(postgres_backend(owner).url(), 1)
        .await
        .unwrap()
        .acquire()
        .await
        .unwrap()
}

/// Take and immediately return the session-level key on the oracle session.
async fn session_free(oracle: &compio_postgres::Client, key: i64) -> bool {
    let acquired: bool = oracle
        .query_one("SELECT pg_try_advisory_lock($1::int8)", &[&key])
        .await
        .unwrap()
        .get(0);
    if acquired {
        let released: bool = oracle
            .query_one("SELECT pg_advisory_unlock($1::int8)", &[&key])
            .await
            .unwrap()
            .get(0);
        assert!(released);
    }
    acquired
}

/// Try the same key as a transaction-level lock in an implicit transaction.
async fn transaction_free(oracle: &compio_postgres::Client, key: i64) -> bool {
    oracle
        .query_one("SELECT pg_try_advisory_xact_lock($1::int8)", &[&key])
        .await
        .unwrap()
        .get(0)
}

/// The backend holding the one-argument key, read from `pg_locks`.
async fn holder(oracle: &compio_postgres::Client, key: i64) -> Option<i32> {
    oracle
        .query_opt(
            "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
             AND objsubid = 1 AND ((classid::bigint << 32) | objid::bigint) = $1",
            &[&key],
        )
        .await
        .unwrap()
        .map(|row| row.get(0))
}

async fn advisory_locks_of(oracle: &compio_postgres::Client, pid: i32) -> i64 {
    oracle
        .query_one(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND pid = $1",
            &[&pid],
        )
        .await
        .unwrap()
        .get(0)
}

/// The pid of the session the ORM pool hands out next.
async fn next_pooled_pid(owner: &CollectionFixture) -> i32 {
    postgres_backend(owner)
        .pool()
        .acquire()
        .await
        .unwrap()
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0)
}

async fn lease(database: &Database, key: i64) -> Option<SessionLease> {
    database
        .postgres()
        .unwrap()
        .try_session_lease(AdvisoryKey::single(key))
        .await
        .unwrap()
}

#[compio::test]
async fn postgres_session_lease_holds_the_exact_one_argument_key() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let held = lease(&owner.database, KEY).await.expect("the key is free");
    assert!(!session_free(&oracle, KEY).await);
    // Control: a distinct key stays free while the lease holds its own.
    assert!(session_free(&oracle, KEY + 1).await);
    held.release().await.unwrap();
    assert!(session_free(&oracle, KEY).await);
    drop(oracle);
    owner.close().await;
}

#[compio::test]
async fn postgres_session_lease_loser_gets_none() {
    let owner = fixture(true, 4).await;
    let held = lease(&owner.database, KEY).await.expect("the key is free");
    assert!(lease(&owner.database, KEY).await.is_none());
    let other_context = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        lease_schema::schema(),
    )
    .unwrap();
    assert!(lease(&other_context, KEY).await.is_none());
    held.release().await.unwrap();
    // Control: the key is acquirable once released.
    lease(&other_context, KEY)
        .await
        .expect("a released key is free")
        .release()
        .await
        .unwrap();
    drop(other_context);
    owner.close().await;
}

#[compio::test]
async fn postgres_session_lease_survives_orm_transactions() {
    let owner = fixture(true, 4).await;
    let oracle = oracle(&owner).await;
    let held = lease(&owner.database, KEY).await.expect("the key is free");
    for (index, commit) in [true, false, true].into_iter().enumerate() {
        let result = owner
            .database
            .transaction(|tx| async move {
                tx.collection("lease_rows")?
                    .insert(value!({"id":format!("row{index}"), "label":"leased"}))
                    .await?;
                if commit {
                    Ok(())
                } else {
                    Err(DbError::validation("test_rollback", "discard the row"))
                }
            })
            .await;
        assert_eq!(result.is_ok(), commit);
        assert!(!session_free(&oracle, KEY).await, "transaction {index}");
    }
    // A transaction-scoped lock on the same key conflicts with the lease.
    assert!(!transaction_free(&oracle, KEY).await);
    held.release().await.unwrap();
    assert!(transaction_free(&oracle, KEY).await);
    drop(oracle);
    owner.close().await;
}

#[compio::test]
async fn postgres_dropped_lease_discards_its_session() {
    let owner = fixture(true, 1).await;
    let oracle = oracle(&owner).await;
    let held = lease(&owner.database, KEY).await.expect("the key is free");
    let pid = holder(&oracle, KEY).await.expect("the lease holds the key");
    drop(held);
    compio::time::timeout(Duration::from_secs(5), async {
        loop {
            let alive: bool = oracle
                .query_one(
                    "SELECT EXISTS (SELECT FROM pg_stat_activity WHERE pid = $1)",
                    &[&pid],
                )
                .await
                .unwrap()
                .get(0);
            if !alive {
                break;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a dropped lease must close its session");
    assert!(session_free(&oracle, KEY).await);
    // Control: a released lease returns its session to the pool of one.
    let held = lease(&owner.database, KEY).await.expect("the key is free");
    let pid = holder(&oracle, KEY).await.expect("the lease holds the key");
    held.release().await.unwrap();
    assert_eq!(next_pooled_pid(&owner).await, pid);
    assert_eq!(advisory_locks_of(&oracle, pid).await, 0);
    drop(oracle);
    owner.close().await;
}

#[compio::test]
async fn postgres_failed_release_never_returns_the_session() {
    let owner = fixture(true, 1).await;
    let oracle = oracle(&owner).await;
    let held = lease(&owner.database, KEY).await.expect("the key is free");
    let pid = holder(&oracle, KEY).await.expect("the lease holds the key");
    let terminated: bool = oracle
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap()
        .get(0);
    assert!(terminated);
    held.release().await.unwrap_err();
    assert_ne!(next_pooled_pid(&owner).await, pid);
    // Control: a normal release succeeds and its session is reused.
    let held = lease(&owner.database, KEY).await.expect("the key is free");
    let pid = holder(&oracle, KEY).await.expect("the lease holds the key");
    held.release().await.unwrap();
    assert_eq!(next_pooled_pid(&owner).await, pid);
    drop(oracle);
    owner.close().await;
}

#[compio::test]
async fn session_lease_refused_on_transaction_receivers_and_sqlite() {
    let owner = fixture(true, 4).await;
    owner
        .database
        .transaction(|tx| async move {
            assert_code(
                tx.postgres()?
                    .try_session_lease(AdvisoryKey::single(KEY))
                    .await
                    .unwrap_err(),
                "session_lease_requires_root",
            );
            Ok(())
        })
        .await
        .unwrap();
    // Control: the root handle takes the lease.
    lease(&owner.database, KEY)
        .await
        .expect("the key is free")
        .release()
        .await
        .unwrap();
    owner.close().await;

    let sqlite = fixture(false, 4).await;
    assert_code(
        sqlite.database.postgres().unwrap_err(),
        "unsupported_backend_feature",
    );
    sqlite.close().await;
}
