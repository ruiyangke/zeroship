use super::*;
use fixtures::CollectionFixture;

async fn exercise(owner: CollectionFixture) {
    let db = &owner.database;
    let tx = db.begin_transaction().await.unwrap();
    let entries = tx.database().collection("entries").unwrap();
    entries.insert(value!({"label":"committed"})).await.unwrap();
    let namespace = db
        .backend
        .namespace(db.binding.app_id(), db.binding.schema());
    let table = format!(
        "{}.\"entries\"",
        crate::sql::compile::quote_ident(namespace)
    );
    let query = CompiledQuery {
        sql: format!("SELECT label FROM {table}"),
        params: Vec::new(),
    };
    assert_eq!(
        tx.query_sql(&query).await.unwrap()[0]["label"],
        value!("committed")
    );
    assert_eq!(
        count(
            db.collection("entries")
                .unwrap()
                .count(value!({}), value!({}))
                .await
                .unwrap()
        ),
        0
    );

    let nested = tx.database().begin_transaction().await.unwrap();
    nested
        .database()
        .collection("entries")
        .unwrap()
        .insert(value!({"label":"nested"}))
        .await
        .unwrap();
    assert!(entries.find(value!({}), value!({})).await.is_err());
    nested.rollback().await.unwrap();
    assert_eq!(tx.query_sql(&query).await.unwrap().len(), 1);
    let prepared = entries.insert(value!({"label":"escaped"}));
    tx.commit().await.unwrap();
    assert!(prepared.await.is_err());
    assert!(entries.find(value!({}), value!({})).await.is_err());

    let tx = db.begin_transaction().await.unwrap();
    tx.execute_sql(&CompiledQuery {
        sql: format!("UPDATE {table} SET label=$1"),
        params: vec![value!("rolled_back")],
    })
    .await
    .unwrap();
    tx.rollback().await.unwrap();

    let tx = db.begin_transaction().await.unwrap();
    let abandoned = tx.database().collection("entries").unwrap();
    abandoned
        .insert(value!({"label":"abandoned"}))
        .await
        .unwrap();
    crate::OrmContext::new().with(|| drop(tx));
    assert!(abandoned.find(value!({}), value!({})).await.is_err());
    compio::time::timeout(std::time::Duration::from_secs(10), async {
        let tx = db.begin_transaction().await.unwrap();
        assert_eq!(
            tx.query_sql(&query).await.unwrap(),
            vec![value!({"label":"committed"})]
        );
        tx.commit().await.unwrap();
    })
    .await
    .expect("abandoning a transaction must release its own admission");
    owner.close().await;
}

#[compio::test]
async fn sqlite_owned_transactions_share_model_and_sql_work() {
    exercise(CollectionFixture::sqlite("entries", value!({"label":{"type":"string"}})).await).await;
}

#[compio::test]
async fn postgres_owned_transactions_share_model_and_sql_work() {
    exercise(CollectionFixture::postgres("entries", value!({"label":{"type":"string"}})).await)
        .await;
}

async fn out_of_order(owner: CollectionFixture) {
    let db = &owner.database;
    let parent = db.begin_transaction().await.unwrap();
    let child = parent.database().begin_transaction().await.unwrap();
    child
        .database()
        .collection("entries")
        .unwrap()
        .insert(value!({"label":"must_roll_back"}))
        .await
        .unwrap();
    assert!(parent.commit().await.is_err());
    assert!(child.commit().await.is_err());
    compio::time::timeout(
        std::time::Duration::from_secs(10),
        db.transaction(|tx| async move {
            assert_eq!(
                count(
                    tx.collection("entries")?
                        .count(value!({}), value!({}))
                        .await?
                ),
                0
            );
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    owner.close().await;
}

#[compio::test]
async fn sqlite_parent_cannot_settle_an_open_child() {
    out_of_order(CollectionFixture::sqlite("entries", value!({"label":{"type":"string"}})).await)
        .await;
}

#[compio::test]
async fn postgres_parent_cannot_settle_an_open_child() {
    out_of_order(CollectionFixture::postgres("entries", value!({"label":{"type":"string"}})).await)
        .await;
}
