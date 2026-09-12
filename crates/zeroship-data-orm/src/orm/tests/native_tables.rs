use super::fixtures::CollectionFixture;
use super::*;

async fn native_tables(postgres: bool) {
    let fields = value!({"label":{"type":"string","unique":true}});
    let mut owner = if postgres {
        CollectionFixture::postgres("records", fields).await
    } else {
        CollectionFixture::sqlite("records", fields).await
    };
    let mut previous = "records";
    for table in [
        "__zeroship_workflow_records",
        "__zeroship_app_metadata",
        "__zs_customer_records",
    ] {
        owner.rename_collection(previous, table).await;
        let database = &owner.database;
        let collection = database.collection(table).unwrap();
        let inserted = collection.insert(value!({"label":"native"})).await.unwrap();
        assert!(matches!(inserted, Output::Rows { .. }));
        collection
            .execute(Operation::Upsert {
                document: value!({"label":"native"}),
                conflict_fields: value!(["label"]),
            })
            .await
            .unwrap();
        assert!(matches!(
            collection
                .execute(Operation::Update {
                    filter: value!({"label":"native"}),
                    patch: value!({"label":"updated"}),
                    many: true,
                })
                .await
                .unwrap(),
            Output::Count(1)
        ));
        let mut query = ReadQuery::new(ReadSource::new(table, "records"));
        query.projection.push(ReadProjection::Row {
            output: "record".into(),
            source: "records".into(),
            fields: None,
            optional: false,
        });
        let Output::Rows { rows, .. } = database.read(query).await.unwrap() else {
            panic!("expected native read rows");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["record"]["label"], value!("updated"));
        let failed: Result<(), DbError> = database
            .transaction(|tx| async move {
                tx.collection(table)?
                    .insert(value!({"label":"rollback"}))
                    .await?;
                Err(DbError::internal("abort native table transaction"))
            })
            .await;
        assert!(failed.is_err());
        assert!(matches!(
            collection
                .execute(Operation::Count {
                    filter: value!({}),
                    options: value!({})
                })
                .await
                .unwrap(),
            Output::Count(1)
        ));
        assert!(database
            .collection(&format!("another_schema.{table}"))
            .is_err());
        assert!(database
            .collection(&format!("{table}\"; DELETE FROM records"))
            .is_err());
        assert!(matches!(
            collection
                .execute(Operation::Purge {
                    filter: value!({}),
                    many: true
                })
                .await
                .unwrap(),
            Output::Count(1)
        ));
        previous = table;
    }
    owner.close().await;
}

#[compio::test]
async fn sqlite_table_access_is_transparent_within_the_bound_schema() {
    native_tables(false).await;
}

#[compio::test]
async fn postgres_table_access_is_transparent_within_the_bound_schema() {
    native_tables(true).await;
}
