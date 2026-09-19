use crate::{
    encryption::ProjectKeySource, error::DbError, orm::Database, value,
    ConnectOptions,
};
use futures::{
    future::{select, Either},
    FutureExt,
};
use std::{num::NonZeroUsize, time::Duration};

#[compio::test]
async fn dropping_native_transaction_interrupts_postgres_before_releasing_admission() {
    let server = crate::tests::fixtures::postgres::Postgres::start();
    let admin = compio_postgres::Pool::connect(&server.url(), 3)
        .await
        .unwrap();
    let app = zeroship_core::AppId::mint();
    let binding = crate::tests::fixtures::harness_binding(app.as_str());
    let schema = crate::sql::mapping::quote_ident(binding.schema().as_str());
    admin
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
         CREATE TABLE {schema}.records (id TEXT PRIMARY KEY, title TEXT NOT NULL);
         CREATE FUNCTION {schema}.wait_at_barrier() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock(73921861); RETURN NEW; END $$;
         CREATE TRIGGER wait_at_barrier BEFORE INSERT ON {schema}.records
         FOR EACH ROW WHEN (NEW.title = 'blocked') EXECUTE FUNCTION {schema}.wait_at_barrier();"
        ))
        .await
        .unwrap();
    let blocker = admin.acquire().await.unwrap();
    blocker
        .batch_execute("SELECT pg_advisory_lock(73921861)")
        .await
        .unwrap();
    let blocker_pid = blocker.process_id();
    let db = Database::connect(
        binding.clone(),
        ConnectOptions::new(server.url(), ProjectKeySource::unavailable())
            .max_connections(NonZeroUsize::new(1).unwrap())
            .connection_authority(),
        crate::schema::Schema::from_collections(vec![(
            "records".into(),
            value!({
                "id":{"type":"string", "primaryKey":true, "required":true},
                "title":{"type":"string", "required":true}
            }),
        )])
        .unwrap(),
    )
    .await
    .unwrap();
    let pending = db
        .transaction(|tx| async move {
            let records = tx.collection("records")?;
            records
                .insert(value!({"id":"before", "title":"rolled back"}))
                .await?;
            records
                .insert(value!({"id":"barrier", "title":"blocked"}))
                .await?;
            Ok::<_, DbError>(())
        })
        .boxed_local();
    let blocked = async {
        loop {
            let rows = admin
                .query(
                    "SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
                    &[&blocker_pid],
                )
                .await
                .unwrap();
            if let Some(row) = rows.first() {
                assert_eq!(rows.len(), 1);
                return row.get::<_, i32>(0);
            }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let (worker, pending) = match select(
        compio::time::timeout(Duration::from_secs(3), blocked).boxed_local(),
        pending,
    )
    .await
    {
        Either::Left((worker, pending)) => (
            worker.expect("statement must reach advisory barrier"),
            pending,
        ),
        Either::Right((result, _)) => panic!("transaction ended before barrier: {result:?}"),
    };
    let other_context = crate::OrmContext::new();
    other_context.with(|| drop(pending));

    let rollback_budget = Duration::from_secs(2);
    assert!(rollback_budget.as_millis() < u128::from(crate::budgets::DB_LOCK_TIMEOUT_MS));
    compio::time::timeout(rollback_budget, async {
        loop {
            let active = admin.query(
                "SELECT EXISTS(SELECT FROM pg_stat_activity WHERE pid = $1 AND xact_start IS NOT NULL)",
                &[&worker],
            ).await.unwrap()[0].get::<_, bool>(0);
            if !active { break; }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("dropped transaction must roll back while the advisory barrier remains held");

    compio::time::timeout(
        rollback_budget,
        db.transaction(|tx| async move {
            assert!(matches!(
                tx.collection("records")?
                    .count(value!({}), value!({}))
                    .await?,
                crate::orm::Output::Count(0)
            ));
            tx.collection("records")?
                .insert(value!({"id":"restart", "title":"committed"}))
                .await?;
            Ok::<_, DbError>(())
        }),
    )
    .await
    .expect("cleanup must release admission for the next transaction")
    .unwrap();
    assert!(blocker
        .query_one("SELECT pg_advisory_unlock(73921861)", &[])
        .await
        .unwrap()
        .get::<_, bool>(0));
    drop(db);
    drop(blocker);
    admin
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
