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
use std::num::NonZeroUsize;

schema! {
    pub settings_schema {
        settings_rows {
            #[orm(primary_key)]
            id: Text,
            label: Text,
        }
    }
}

const COLUMNS: &str = "id TEXT PRIMARY KEY, label TEXT NOT NULL, \
     flag TEXT NOT NULL DEFAULT COALESCE(current_setting('test_ns.flag', true), ''), \
     pid INTEGER NOT NULL DEFAULT pg_backend_pid()";

async fn fixture() -> CollectionFixture {
    let fields = value!({
        "id": {"type":"string", "primaryKey":true, "required":true},
        "label": {"type":"string", "required":true}
    });
    let mut owner =
        CollectionFixture::postgres_from_table_definition("settings_rows", fields, COLUMNS).await;
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        settings_schema::schema(),
    )
    .unwrap();
    owner
}

fn postgres_backend(owner: &CollectionFixture) -> &crate::backend::postgres::PostgresBackend {
    owner
        .database
        .backend
        .get::<crate::backend::postgres::PostgresBackend>()
        .unwrap()
}

/// A second handle over the fixture's database whose connection declares
/// the `test_ns` setting namespace.
async fn declared(owner: &CollectionFixture, pool_size: usize) -> Database {
    Database::connect(
        owner.database.binding.clone(),
        crate::ConnectOptions::new(
            postgres_backend(owner).url(),
            ProjectKeySource::unavailable(),
        )
        .max_connections(NonZeroUsize::new(pool_size).unwrap())
        .transaction_setting_namespace("test_ns"),
        settings_schema::schema(),
    )
    .await
    .unwrap()
}

fn flag() -> TransactionSetting {
    TransactionSetting::new("test_ns.flag").unwrap()
}

fn assert_code(error: DbError, expected: &str) {
    let matched =
        matches!(&error, DbError::ValidationFailed { code, .. } if *code == expected);
    assert!(matched, "expected {expected}: {}", error.into_string());
}

/// The committed row's defaulted setting and backend pid, read outside the ORM.
async fn observed(owner: &CollectionFixture, id: &str) -> Option<(String, i32)> {
    let table = format!(
        "{}.settings_rows",
        crate::sql::mapping::quote_ident(owner.database.binding.schema().as_str())
    );
    postgres_backend(owner)
        .pool()
        .acquire()
        .await
        .unwrap()
        .query_opt(&format!("SELECT flag, pid FROM {table} WHERE id = $1"), &[&id])
        .await
        .unwrap()
        .map(|row| (row.get(0), row.get(1)))
}

async fn insert(database: &Database, id: &str) -> Result<(), DbError> {
    database
        .collection("settings_rows")?
        .insert(value!({"id":id, "label":"row"}))
        .await
        .map(drop)
}

#[compio::test]
async fn postgres_transaction_setting_is_visible_only_inside_its_transaction() {
    let owner = fixture().await;
    let db = declared(&owner, 4).await;
    db.transaction(|tx| async move {
        insert(&tx, "before").await?;
        tx.postgres()?.set_local(&flag(), "on").await?;
        insert(&tx, "inside").await
    })
    .await
    .unwrap();
    db.transaction(|tx| async move { insert(&tx, "next").await })
        .await
        .unwrap();
    assert_eq!(observed(&owner, "inside").await.unwrap().0, "on");
    // Controls: the same transaction before the call, and the next transaction.
    assert_eq!(observed(&owner, "before").await.unwrap().0, "");
    assert_eq!(observed(&owner, "next").await.unwrap().0, "");
    drop(db);
    owner.close().await;
}

#[compio::test]
async fn postgres_trigger_gated_delete_mirrors_audit_retention() {
    let owner = fixture().await;
    let schema = crate::sql::mapping::quote_ident(owner.database.binding.schema().as_str());
    postgres_backend(&owner)
        .pool()
        .batch_execute(&format!(
            "CREATE FUNCTION {schema}.guard_delete() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF current_setting('test_ns.flag', true) IS DISTINCT FROM 'on' THEN \
                 RAISE EXCEPTION 'retention delete refused'; \
               END IF; \
               RETURN OLD; \
             END $$; \
             CREATE TRIGGER guard BEFORE DELETE ON {schema}.settings_rows \
               FOR EACH ROW EXECUTE FUNCTION {schema}.guard_delete(); \
             INSERT INTO {schema}.settings_rows (id, label) VALUES \
               ('r1', 'row'), ('r2', 'row'), ('r3', 'row')"
        ))
        .await
        .unwrap();
    let db = declared(&owner, 4).await;
    let delete = |id: &'static str, enable: bool| {
        db.transaction(move |tx| async move {
            if enable {
                tx.postgres()?.set_local(&flag(), "on").await?;
            }
            tx.collection("settings_rows")?
                .delete(value!({"id":id}))
                .await
                .map(drop)
        })
    };
    let refused = |error: DbError| {
        assert!(
            matches!(&error, DbError::Internal { message } if message.contains("retention delete refused")),
            "{error:?}"
        );
    };
    refused(delete("r1", false).await.unwrap_err());
    delete("r1", true).await.unwrap();
    assert!(observed(&owner, "r1").await.is_none());
    // After commit the next transaction is refused again.
    refused(delete("r2", false).await.unwrap_err());
    // After rollback.
    let rolled_back = db
        .transaction(|tx| async move {
            tx.postgres()?.set_local(&flag(), "on").await?;
            tx.collection("settings_rows")?
                .delete(value!({"id":"r2"}))
                .await?;
            Err::<(), _>(DbError::validation("test_rollback", "discard the delete"))
        })
        .await;
    assert_code(rolled_back.unwrap_err(), "test_rollback");
    assert!(observed(&owner, "r2").await.is_some());
    refused(delete("r2", false).await.unwrap_err());
    // After cancellation.
    let (enabled, ready) = oneshot::channel();
    let cancelled = db
        .transaction(|tx| async move {
            tx.postgres()?.set_local(&flag(), "on").await?;
            enabled.send(()).unwrap();
            std::future::pending::<()>().await;
            Ok::<_, DbError>(())
        })
        .boxed_local();
    match select(ready, cancelled).await {
        Either::Left((ready, cancelled)) => {
            ready.unwrap();
            drop(cancelled);
        }
        Either::Right((result, _)) => panic!("the setting callback ended early: {result:?}"),
    }
    refused(delete("r3", false).await.unwrap_err());
    assert!(observed(&owner, "r3").await.is_some());
    drop(db);
    owner.close().await;
}

#[compio::test]
async fn postgres_transaction_setting_never_reaches_the_pool() {
    let owner = fixture().await;
    let db = declared(&owner, 1).await;
    db.transaction(|tx| async move {
        tx.postgres()?.set_local(&flag(), "on").await?;
        insert(&tx, "inside").await
    })
    .await
    .unwrap();
    insert(&db, "after").await.unwrap();
    let (inside, inside_pid) = observed(&owner, "inside").await.unwrap();
    let (after, after_pid) = observed(&owner, "after").await.unwrap();
    assert_eq!(inside_pid, after_pid, "a pool of one reuses the session");
    // Control: the value was visible inside its transaction.
    assert_eq!(inside, "on");
    assert_eq!(after, "");
    drop(db);
    owner.close().await;
}

#[compio::test]
async fn postgres_savepoint_rollback_restores_the_outer_value() {
    let owner = fixture().await;
    let db = declared(&owner, 4).await;
    db.transaction(|tx| async move {
        tx.postgres()?.set_local(&flag(), "a").await?;
        let nested = tx
            .transaction(|inner| async move {
                inner.postgres()?.set_local(&flag(), "b").await?;
                insert(&inner, "nested").await?;
                Err::<(), _>(DbError::validation("test_rollback", "roll back the frame"))
            })
            .await;
        assert_code(nested.unwrap_err(), "test_rollback");
        insert(&tx, "after_rollback").await?;
        // Control: a released frame keeps its value.
        tx.transaction(|inner| async move { inner.postgres()?.set_local(&flag(), "b").await })
            .await?;
        insert(&tx, "after_release").await
    })
    .await
    .unwrap();
    assert!(observed(&owner, "nested").await.is_none());
    assert_eq!(observed(&owner, "after_rollback").await.unwrap().0, "a");
    assert_eq!(observed(&owner, "after_release").await.unwrap().0, "b");
    drop(db);
    owner.close().await;
}

#[compio::test]
async fn transaction_setting_names_are_validated_before_sql() {
    for name in [
        "statement_timeout",
        "role",
        "search_path",
        "lock_timeout",
        "Test_ns.flag",
        "test_ns.flag;x",
    ] {
        assert_code(
            TransactionSetting::new(name).unwrap_err(),
            "invalid_transaction_setting",
        );
    }
    let owner = fixture().await;
    let db = declared(&owner, 4).await;
    let exact = "it's; \\ \"quoted\" -- $$";
    db.transaction(|tx| async move {
        let postgres = tx.postgres()?;
        assert_code(
            postgres
                .set_local(&TransactionSetting::new("other.flag")?, "on")
                .await
                .unwrap_err(),
            "invalid_transaction_setting",
        );
        assert_code(
            postgres.set_local(&flag(), "nul\0byte").await.unwrap_err(),
            "invalid_transaction_setting",
        );
        // Control: the declared name accepts a value byte for byte, and the
        // refusals above left the transaction usable.
        postgres.set_local(&flag(), exact).await?;
        insert(&tx, "exact").await
    })
    .await
    .unwrap();
    assert_eq!(observed(&owner, "exact").await.unwrap().0, exact);
    // A connection that declared no namespace refuses every setting.
    owner
        .database
        .transaction(|tx| async move {
            assert_code(
                tx.postgres()?.set_local(&flag(), "on").await.unwrap_err(),
                "invalid_transaction_setting",
            );
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    drop(db);
    owner.close().await;
}

#[compio::test]
async fn set_local_requires_the_transaction_receiver_and_postgres() {
    let owner = fixture().await;
    let db = declared(&owner, 4).await;
    assert_code(
        db.postgres()
            .unwrap()
            .set_local(&flag(), "on")
            .await
            .unwrap_err(),
        "transaction_required",
    );
    // Control: the transaction handle is accepted.
    db.transaction(|tx| async move { tx.postgres()?.set_local(&flag(), "on").await })
        .await
        .unwrap();
    drop(db);
    owner.close().await;

    let directory = tempfile::tempdir().unwrap();
    let sqlite = Database::connect(
        crate::tests::fixtures::harness_binding(zeroship_core::app_id::AppId::mint().as_str()),
        crate::ConnectOptions::new(
            directory.path().join("settings.sqlite").to_string_lossy(),
            ProjectKeySource::unavailable(),
        )
        .transaction_setting_namespace("test_ns"),
        settings_schema::schema(),
    )
    .await
    .unwrap();
    sqlite
        .transaction(|tx| async move {
            assert_code(tx.postgres().unwrap_err(), "unsupported_backend_feature");
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
}

#[compio::test]
async fn declared_setting_namespaces_are_lowercase_identifiers() {
    let directory = tempfile::tempdir().unwrap();
    let url = directory.path().join("namespaces.sqlite");
    let connect = |namespace: &'static str| {
        crate::ConnectOptions::new(url.to_string_lossy(), ProjectKeySource::unavailable())
            .transaction_setting_namespace(namespace)
            .connect()
    };
    for namespace in ["Test_ns", "test-ns", "test.ns", "", "9ns"] {
        let error = connect(namespace).await.unwrap_err();
        assert!(
            matches!(&error, DbError::Configuration { code, .. } if *code == "invalid_transaction_setting_namespace"),
            "{namespace}: {error:?}"
        );
    }
    // Control: a lowercase identifier is accepted.
    connect("test_ns").await.unwrap();
}
