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
    pub lock_schema {
        lock_rows {
            #[orm(primary_key)]
            id: Text,
            label: Text,
        }
        lock_views {
            #[orm(primary_key)]
            id: Text,
            label: Text,
        }
    }
}
use lock_schema::{lock_rows, lock_views};

#[derive(Debug, FromRow)]
#[orm(entity = lock_rows)]
struct LockedRow {
    id: String,
    label: String,
}

#[derive(Debug, FromRow)]
#[orm(entity = lock_views)]
struct ViewRow {
    id: String,
    label: String,
}

async fn fixture(postgres: bool) -> CollectionFixture {
    let fields = value!({
        "id": {"type":"string", "primaryKey":true, "required":true},
        "label": {"type":"string", "required":true}
    });
    let columns = "id TEXT PRIMARY KEY, label TEXT NOT NULL";
    let mut owner = if postgres {
        CollectionFixture::postgres_from_table_definition("lock_rows", fields, columns).await
    } else {
        CollectionFixture::sqlite_from_table_definition("lock_rows", fields, columns).await
    };
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        lock_schema::schema(),
    )
    .unwrap();
    for id in ["held", "free"] {
        owner
            .database
            .collection("lock_rows")
            .unwrap()
            .insert(value!({"id":id,"label":"original"}))
            .await
            .unwrap();
    }
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

fn qualified(owner: &CollectionFixture, table: &str) -> String {
    format!(
        "{}.{table}",
        crate::sql::mapping::quote_ident(owner.database.binding.schema().as_str())
    )
}

fn lock_not_available(result: Result<compio_postgres::Row, compio_postgres::Error>) -> bool {
    result.is_err_and(|error| {
        error.code().map(compio_postgres::error::SqlState::code) == Some("55P03")
    })
}

async fn requires_transaction(postgres: bool) {
    let owner = fixture(postgres).await;
    let root = owner.database.clone();
    assert_code(
        root.entity::<lock_rows::Entity>()
            .unwrap()
            .query()
            .for_update()
            .unwrap_err(),
        "transaction_required",
    );
    owner
        .database
        .transaction(|tx| async move {
            let alias = root.entity::<lock_rows::Entity>()?.alias("root")?;
            assert_code(
                root.from(&alias).for_update().unwrap_err(),
                "transaction_required",
            );
            assert_code(
                root.from(&alias).for_update_of(&alias).unwrap_err(),
                "transaction_required",
            );
            // Control: the identical builders on the transaction handle are accepted.
            let inside = tx.entity::<lock_rows::Entity>()?.alias("inside")?;
            assert!(tx.from(&inside).for_update().is_ok());
            assert!(tx.from(&inside).for_update_of(&inside).is_ok());
            assert!(tx.entity::<lock_rows::Entity>()?.query().for_update().is_ok());
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    owner.close().await;
}

#[compio::test]
async fn postgres_row_locks_require_the_transaction_receiver() {
    requires_transaction(true).await;
}

#[compio::test]
async fn sqlite_row_locks_require_the_transaction_receiver() {
    requires_transaction(false).await;
}

#[compio::test]
async fn sqlite_refuses_required_row_locks_without_poisoning_the_transaction() {
    let owner = fixture(false).await;
    owner
        .database
        .transaction(|tx| async move {
            let table = tx.entity::<lock_rows::Entity>()?;
            assert_code(
                table
                    .query()
                    .for_update()?
                    .all::<LockedRow>()
                    .await
                    .unwrap_err(),
                "unsupported_backend_feature",
            );
            let alias = table.alias("r")?;
            assert_code(
                tx.from(&alias)
                    .for_update_of(&alias)?
                    .select(alias.row::<LockedRow>())?
                    .all()
                    .await
                    .unwrap_err(),
                "unsupported_backend_feature",
            );
            // Control: the same transaction keeps reading and writing.
            assert_eq!(table.query().all::<LockedRow>().await?.len(), 2);
            tx.collection("lock_rows")?
                .insert(value!({"id":"after", "label":"committed"}))
                .await?;
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    let committed = owner
        .database
        .entity::<lock_rows::Entity>()
        .unwrap()
        .query()
        .filter(lock_rows::id.eq("after").unwrap())
        .first::<LockedRow>()
        .await
        .unwrap();
    assert_eq!(committed.unwrap().label, "committed");
    owner.close().await;
}

async fn wait_for_lock(admin: &compio_postgres::Client, pid: i32) {
    compio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = admin
                .query_one(
                    "SELECT wait_event_type = 'Lock' FROM pg_stat_activity WHERE pid = $1",
                    &[&pid],
                )
                .await
                .unwrap()
                .get::<_, Option<bool>>(0)
                .unwrap_or(false);
            if waiting {
                return;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the competing write must reach its row-lock wait");
}

/// Wait until some other session's locking read waits on a row lock.
async fn wait_for_locking_read(admin: &compio_postgres::Client) {
    compio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = admin
                .query_one(
                    "SELECT EXISTS (SELECT FROM pg_stat_activity WHERE pid <> pg_backend_pid() \
                     AND wait_event_type = 'Lock' AND query LIKE '%FOR UPDATE%')",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if waiting {
                return;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the ORM locking read must reach its row-lock wait");
}

#[compio::test]
async fn postgres_row_locks_block_competing_writes_until_settlement() {
    #[derive(Clone, Copy)]
    enum Settlement {
        Commit,
        Rollback,
        Cancel,
    }

    let owner = fixture(true).await;
    let backend = postgres_backend(&owner);
    let admin = backend.pool().acquire().await.unwrap();
    let peer = backend.pool().acquire().await.unwrap();
    let pid: i32 = peer
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let table = qualified(&owner, "lock_rows");
    for settlement in [Settlement::Commit, Settlement::Rollback, Settlement::Cancel] {
        let (held, ready) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        let holding = owner
            .database
            .transaction(|tx| async move {
                let row = tx
                    .entity::<lock_rows::Entity>()?
                    .query()
                    .filter(lock_rows::id.eq("held")?)
                    .for_update()?
                    .first::<LockedRow>()
                    .await?
                    .unwrap();
                assert_eq!(row.id, "held");
                assert!(!row.label.is_empty());
                held.send(()).unwrap();
                resume.await.unwrap();
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
        assert_eq!(
            admin
                .execute(
                    &format!("UPDATE {table} SET label = 'unblocked' WHERE id = 'free'"),
                    &[]
                )
                .await
                .unwrap(),
            1
        );
        let sql = format!("UPDATE {table} SET label = 'peer' WHERE id = 'held'");
        let write = peer.execute(&sql, &[]).boxed_local();
        let write = match select(wait_for_lock(&admin, pid).boxed_local(), write).await {
            Either::Left(((), write)) => write,
            Either::Right((result, _)) => panic!("write bypassed the held row lock: {result:?}"),
        };
        if matches!(settlement, Settlement::Cancel) {
            drop(holding);
            drop(release);
            assert_eq!(
                compio::time::timeout(Duration::from_secs(5), write)
                    .await
                    .expect("cancelled callback must release its row lock")
                    .unwrap(),
                1
            );
        } else {
            release.send(()).unwrap();
            let (settled, written) = futures::future::join(holding, write).await;
            assert_eq!(settled.is_err(), matches!(settlement, Settlement::Rollback));
            assert_eq!(written.unwrap(), 1);
        }
    }
    drop(peer);
    drop(admin);
    owner.close().await;
}

#[compio::test]
async fn postgres_for_update_of_locks_only_the_selected_alias() {
    let owner = fixture(true).await;
    let peer_connection = postgres_backend(&owner).pool().acquire().await.unwrap();
    let peer = &peer_connection;
    let table = &qualified(&owner, "lock_rows");
    owner
        .database
        .transaction(|tx| async move {
            let entity = tx.entity::<lock_rows::Entity>()?;
            let held = entity.alias("held")?;
            let free = entity.alias("free")?;
            let rows = tx
                .from(&held)
                .inner_join(&free, free.column(lock_rows::id).eq("free")?)?
                .filter(held.column(lock_rows::id).eq("held")?)
                .for_update_of(&held)?
                .select((held.row::<LockedRow>(), free.row::<LockedRow>()))?
                .all()
                .await?;
            assert_eq!(rows.len(), 1);
            assert_eq!((rows[0].0.id.as_str(), rows[0].1.id.as_str()), ("held", "free"));
            // Control: the joined alias's row stays lockable.
            assert!(peer
                .query_one(
                    &format!("SELECT id FROM {table} WHERE id = 'free' FOR UPDATE NOWAIT"),
                    &[]
                )
                .await
                .is_ok());
            assert!(lock_not_available(
                peer.query_one(
                    &format!("SELECT id FROM {table} WHERE id = 'held' FOR UPDATE NOWAIT"),
                    &[],
                )
                .await
            ));
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    drop(peer_connection);
    owner.close().await;
}

#[compio::test]
async fn postgres_for_update_of_needs_update_privilege_only_on_the_locked_source() {
    let owner = fixture(true).await;
    let backend = postgres_backend(&owner);
    let schema = owner.database.binding.schema().as_str().to_owned();
    let role = crate::sql::mapping::quote_ident(
        &zeroship_core::database_role::per_app_role_name(&schema).unwrap(),
    );
    let views = qualified(&owner, "lock_views");
    backend
        .pool()
        .batch_execute(&format!(
            "CREATE TABLE {views} (id TEXT PRIMARY KEY, label TEXT NOT NULL); \
             INSERT INTO {views} VALUES ('held', 'view'); \
             REVOKE ALL ON {views} FROM {role}; \
             GRANT SELECT ON {views} TO {role}"
        ))
        .await
        .unwrap();
    let privileges: (bool, bool) = backend
        .pool()
        .acquire()
        .await
        .unwrap()
        .query_one(
            &format!(
                "SELECT has_table_privilege($1, '{views}', 'SELECT'), \
                 has_table_privilege($1, '{views}', 'UPDATE')"
            ),
            &[&zeroship_core::database_role::per_app_role_name(&schema).unwrap()],
        )
        .await
        .map(|row| (row.get(0), row.get(1)))
        .unwrap();
    assert_eq!(privileges, (true, false), "the joined table must be select-only");
    let read = |qualified_lock: bool| {
        owner.database.transaction(move |tx| async move {
            let rows = tx.entity::<lock_rows::Entity>()?.alias("r")?;
            let views = tx.entity::<lock_views::Entity>()?.alias("v")?;
            let builder = tx.from(&rows).inner_join(
                &views,
                views.column(lock_views::id).eq(rows.column(lock_rows::id))?,
            )?;
            let builder = if qualified_lock {
                builder.for_update_of(&rows)?
            } else {
                builder.for_update()?
            };
            builder
                .select((rows.row::<LockedRow>(), views.row::<ViewRow>()))?
                .all()
                .await
        })
    };
    let locked = read(true).await.unwrap();
    assert_eq!(locked.len(), 1);
    assert_eq!(
        (
            locked[0].0.label.as_str(),
            locked[0].1.id.as_str(),
            locked[0].1.label.as_str()
        ),
        ("original", "held", "view")
    );
    // Control: locking every source also needs UPDATE on the select-only table.
    let refused = read(false).await.unwrap_err();
    assert!(
        matches!(&refused, DbError::Internal { message } if message.contains("permission denied")),
        "{refused:?}"
    );
    owner.close().await;
}

#[compio::test]
async fn postgres_locking_read_waits_and_returns_the_latest_committed_row_under_read_committed() {
    let owner = fixture(true).await;
    let backend = postgres_backend(&owner);
    let admin = backend.pool().acquire().await.unwrap();
    let holder = backend.pool().acquire().await.unwrap();
    let table = qualified(&owner, "lock_rows");
    holder
        .batch_execute(&format!(
            "BEGIN; UPDATE {table} SET label = 'committed' WHERE id = 'held'"
        ))
        .await
        .unwrap();
    // Control: a plain read during the hold returns the old row without waiting.
    let plain = compio::time::timeout(
        Duration::from_secs(5),
        owner
            .database
            .entity::<lock_rows::Entity>()
            .unwrap()
            .query()
            .filter(lock_rows::id.eq("held").unwrap())
            .first::<LockedRow>(),
    )
    .await
    .expect("a plain read must not wait for the row lock")
    .unwrap()
    .unwrap();
    assert_eq!(plain.label, "original");
    let waiting = owner
        .database
        .transaction_with_options(
            TransactionOptions::default().isolation_level(IsolationLevel::ReadCommitted),
            |tx| async move {
                tx.entity::<lock_rows::Entity>()?
                    .query()
                    .filter(lock_rows::id.eq("held")?)
                    .for_update()?
                    .first::<LockedRow>()
                    .await
            },
        )
        .boxed_local();
    let waiting = match select(wait_for_locking_read(&admin).boxed_local(), waiting).await {
        Either::Left(((), waiting)) => waiting,
        Either::Right((result, _)) => panic!("the locking read did not wait: {result:?}"),
    };
    holder.batch_execute("COMMIT").await.unwrap();
    let row = compio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the locking read must resume after the holder commits")
        .unwrap()
        .unwrap();
    assert_eq!(row.label, "committed");
    drop(holder);
    drop(admin);
    owner.close().await;
}

#[compio::test]
async fn postgres_locking_read_under_repeatable_read_reports_serialization() {
    let owner = fixture(true).await;
    let peer = postgres_backend(&owner).pool().acquire().await.unwrap();
    let table = qualified(&owner, "lock_rows");
    let interleave = |level: IsolationLevel, label: &'static str| {
        let peer = &peer;
        let table = &table;
        owner.database.transaction_with_options(
            TransactionOptions::default().isolation_level(level),
            move |tx| async move {
                let rows = tx.entity::<lock_rows::Entity>()?;
                // The first read fixes a repeatable-read snapshot.
                rows.query()
                    .filter(lock_rows::id.eq("free")?)
                    .first::<LockedRow>()
                    .await?;
                peer.execute(
                    &format!("UPDATE {table} SET label = '{label}' WHERE id = 'held'"),
                    &[],
                )
                .await
                .unwrap();
                rows.query()
                    .filter(lock_rows::id.eq("held")?)
                    .for_update()?
                    .first::<LockedRow>()
                    .await
            },
        )
    };
    let error = interleave(IsolationLevel::RepeatableRead, "concurrent")
        .await
        .unwrap_err();
    assert!(
        matches!(error, DbError::Serialization { .. }),
        "{error:?}"
    );
    // Control: read committed locks and returns the latest committed version.
    let row = interleave(IsolationLevel::ReadCommitted, "latest")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.label, "latest");
    drop(peer);
    owner.close().await;
}

#[compio::test]
async fn postgres_keyset_locking_pages_accumulate_until_settlement() {
    let owner = fixture(true).await;
    let peer_connection = postgres_backend(&owner).pool().acquire().await.unwrap();
    let peer = &peer_connection;
    let table = &qualified(&owner, "lock_rows");
    for id in ["p1", "p2", "p3", "p4", "z9"] {
        owner
            .database
            .collection("lock_rows")
            .unwrap()
            .insert(value!({"id":id, "label":"page"}))
            .await
            .unwrap();
    }
    owner
        .database
        .transaction(|tx| async move {
            let rows = tx.entity::<lock_rows::Entity>()?;
            let mut locked = Vec::new();
            let mut after = String::from("p");
            loop {
                let page = rows
                    .query()
                    .filter(lock_rows::id.gt(after.as_str())?.and(lock_rows::id.lt("q")?))
                    .order_by(lock_rows::id.asc())
                    .limit(2)?
                    .for_update()?
                    .all::<LockedRow>()
                    .await?;
                let short = page.len() < 2;
                if let Some(last) = page.last() {
                    after.clone_from(&last.id);
                }
                locked.extend(page.into_iter().map(|row| row.id));
                if short {
                    break;
                }
            }
            assert_eq!(locked, ["p1", "p2", "p3", "p4"]);
            for id in ["p1", "p3"] {
                assert!(
                    lock_not_available(
                        peer.query_one(
                            &format!("SELECT id FROM {table} WHERE id = '{id}' FOR UPDATE NOWAIT"),
                            &[],
                        )
                        .await
                    ),
                    "{id} must stay locked by its page"
                );
            }
            // Control: a row outside the key range stays lockable.
            assert!(peer
                .query_one(
                    &format!("SELECT id FROM {table} WHERE id = 'z9' FOR UPDATE NOWAIT"),
                    &[]
                )
                .await
                .is_ok());
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    // Every page released at settlement.
    assert!(peer
        .query_one(
            &format!("SELECT id FROM {table} WHERE id = 'p1' FOR UPDATE NOWAIT"),
            &[]
        )
        .await
        .is_ok());
    drop(peer_connection);
    owner.close().await;
}

#[compio::test]
async fn postgres_row_lock_in_rolled_back_savepoint() {
    let owner = fixture(true).await;
    let peer_connection = postgres_backend(&owner).pool().acquire().await.unwrap();
    let peer = &peer_connection;
    let table = &qualified(&owner, "lock_rows");
    owner
        .database
        .transaction(|tx| async move {
            tx.entity::<lock_rows::Entity>()?
                .query()
                .filter(lock_rows::id.eq("held")?)
                .for_update()?
                .first::<LockedRow>()
                .await?;
            let nested = tx
                .transaction(|inner| async move {
                    inner
                        .entity::<lock_rows::Entity>()?
                        .query()
                        .filter(lock_rows::id.eq("free")?)
                        .for_update()?
                        .first::<LockedRow>()
                        .await?;
                    Err::<(), _>(DbError::validation("test_rollback", "roll back the frame"))
                })
                .await;
            assert_code(nested.unwrap_err(), "test_rollback");
            // Observed contract: the savepoint's rollback released its row lock.
            assert!(peer
                .query_one(
                    &format!("SELECT id FROM {table} WHERE id = 'free' FOR UPDATE NOWAIT"),
                    &[]
                )
                .await
                .is_ok());
            // Control: the root frame's lock survives the nested rollback.
            assert!(lock_not_available(
                peer.query_one(
                    &format!("SELECT id FROM {table} WHERE id = 'held' FOR UPDATE NOWAIT"),
                    &[],
                )
                .await
            ));
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    drop(peer_connection);
    owner.close().await;
}

#[compio::test]
async fn postgres_row_locks_reject_summary_grouping_and_nullable_join_targets() {
    let owner = fixture(true).await;
    let foreign = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        lock_schema::schema(),
    )
    .unwrap();
    owner
        .database
        .transaction(|tx| async move {
            let entity = tx.entity::<lock_rows::Entity>()?;
            assert_code(
                entity.query().for_update()?.count().await.unwrap_err(),
                "invalid_read",
            );
            assert_code(
                entity.query().for_update()?.exists().await.unwrap_err(),
                "invalid_read",
            );
            let left = entity.alias("left")?;
            let right = entity.alias("right")?;
            assert_code(
                tx.from(&left)
                    .for_update()?
                    .select(count_rows())?
                    .all()
                    .await
                    .unwrap_err(),
                "invalid_read",
            );
            assert_code(
                tx.from(&left)
                    .group_by(left.column(lock_rows::label))
                    .for_update()?
                    .select((
                        left.column(lock_rows::label).select::<String>(),
                        count_rows(),
                    ))?
                    .all()
                    .await
                    .unwrap_err(),
                "invalid_read",
            );
            assert_code(
                tx.from(&left)
                    .left_join(&right, right.column(lock_rows::id).eq("missing")?)?
                    .for_update()?
                    .select(left.row::<LockedRow>())?
                    .all()
                    .await
                    .unwrap_err(),
                "invalid_read",
            );
            assert_code(
                tx.from(&left)
                    .left_join(&right, right.column(lock_rows::id).eq("missing")?)?
                    .for_update_of(&right)?
                    .select(left.row::<LockedRow>())?
                    .all()
                    .await
                    .unwrap_err(),
                "invalid_read",
            );
            // Control: the non-nullable source of a left join can be locked.
            let valid = tx
                .from(&left)
                .left_join(&right, right.column(lock_rows::id).eq("missing")?)?
                .for_update_of(&left)?
                .select(left.row::<LockedRow>())?
                .all()
                .await?;
            assert_eq!(valid.len(), 2);
            let unknown = entity.alias("unknown")?;
            assert_code(
                tx.from(&left).for_update_of(&unknown).unwrap_err(),
                "invalid_read",
            );
            let foreign = foreign.entity::<lock_rows::Entity>()?.alias("left")?;
            assert_code(
                tx.from(&left).for_update_of(&foreign).unwrap_err(),
                "invalid_read",
            );
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    owner.close().await;
}

#[compio::test]
async fn postgres_row_lock_handles_and_unpolled_reads_expire_with_the_callback() {
    let owner = fixture(true).await;
    let (expired, pending) = owner
        .database
        .transaction(|tx| async move {
            let pending = tx
                .entity::<lock_rows::Entity>()?
                .query()
                .for_update()?
                .all::<LockedRow>();
            Ok::<_, DbError>((tx, pending))
        })
        .await
        .unwrap();
    owner
        .database
        .transaction(|replacement| async move {
            assert_code(pending.await.unwrap_err(), "transaction_scope_expired");
            assert_eq!(
                replacement
                    .entity::<lock_rows::Entity>()?
                    .query()
                    .all::<LockedRow>()
                    .await?
                    .len(),
                2
            );
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    assert_code(
        expired.entity::<lock_rows::Entity>().unwrap_err(),
        "transaction_scope_expired",
    );
    owner.close().await;
}
