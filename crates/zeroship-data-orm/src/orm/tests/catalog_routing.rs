use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "id":{"type":"string", "primaryKey":true, "required":true},
        "label":{"type":"string"}
    })
}

fn fields_with_unprotected_secret() -> Value {
    value!({
        "id":{"type":"string", "primaryKey":true, "required":true},
        "label":{"type":"string"},
        "secret":{"type":"string"}
    })
}

#[compio::test]
async fn postgres_cold_catalog_reads_use_the_held_transaction_connection() {
    let owner = CollectionFixture::postgres_from_table_definition_with_pool_size(
        "records",
        fields(),
        "id TEXT PRIMARY KEY, label TEXT",
        1,
    )
    .await;
    cold_transaction(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn sqlite_cold_catalog_reads_use_the_reserved_transaction() {
    let owner = CollectionFixture::sqlite_from_table_definition(
        "records",
        fields(),
        "id TEXT PRIMARY KEY, label TEXT",
    )
    .await;
    cold_transaction(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn sqlite_catalog_reads_transaction_local_protection() {
    use crate::protection::Catalog;

    let owner = CollectionFixture::sqlite_from_table_definition(
        "records",
        fields(),
        "id TEXT PRIMARY KEY, label TEXT",
    )
    .await;
    let backend = owner
        .database
        .backend
        .get_rc::<crate::backend::sqlite::SqliteBackend>()
        .expect("SQLite fixture backend");
    let app_id = owner.database.binding.app_id().to_owned();
    let schema = owner.database.binding.schema().clone();
    let database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        Schema::from_collections(vec![("protected_records".into(), fields_with_unprotected_secret())]).unwrap(),
    )
    .unwrap();

    let error = database
        .transaction(|tx| async move {
            let route = tx.capture_route().bind(tx.backend.clone())?;
            crate::exec::run_statement(
                &route,
                &format!(
                    "CREATE TABLE {}.protected_records (\
                        \"id\" TEXT PRIMARY KEY, \
                        \"label\" TEXT, \
                        \"secret\" TEXT /* zero-migrate:mask:kind=full,classification=pii */\
                     )",
                    crate::sql::mapping::quote_ident(&app_id),
                ),
                &[],
            )
            .await?;

            let committed = backend.introspect_schema(&app_id, &schema, None).await?;
            assert!(
                !committed.tables.contains_key("protected_records"),
                "the autocommit connection must not see transaction-local catalog changes"
            );
            let transactional = crate::exec::read_catalog(&route).await?;
            assert!(
                transactional
                    .tables
                    .get("protected_records")
                    .and_then(|columns| columns.get("secret"))
                    .is_some_and(|column| column.mask.is_some()),
                "the reserved connection must see transaction-local protection: {transactional:?}"
            );

            tx.collection("protected_records")?
                .insert(value!({"id":"refused", "label":"cold", "secret":"private"}))
                .await
        })
        .await
        .expect_err("transaction-local catalog protection must reject plaintext");
    assert!(
        matches!(
            error,
            DbError::Configuration {
                code: "protection_removed_from_descriptor",
                ..
            }
        ),
        "{error:?}"
    );
    owner.close().await;
}

async fn cold_transaction(database: &Database) {
    compio::time::timeout(
        std::time::Duration::from_secs(3),
        database.transaction(|tx| async move {
            tx.collection("records")?
                .insert(value!({"id":"committed", "label":"cold"}))
                .await
        }),
    )
    .await
    .expect("cold catalog reads must not wait for a second pool connection")
    .unwrap();
    let Output::Rows { rows, .. } = database
        .collection("records")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(rows, vec![value!({"id":"committed", "label":"cold"})]);
}

#[compio::test]
async fn postgres_catalog_protection_follows_the_bound_schema() {
    let owner = CollectionFixture::postgres(
        "records",
        value!({
            "label":{"type":"string", "mask":{"kind":"full", "classification":"pii"}}
        }),
    )
    .await;
    for in_transaction in [false, true] {
        let database = Database::from_schema(
            DbBinding::new(
                zeroship_core::AppId::mint().as_str(),
                "another_deploy",
                owner.database.binding.schema().clone(),
            ),
            owner.database.backend.clone(),
            Schema::from_collections(vec![("records".into(), fields())]).unwrap(),
        )
        .unwrap();
        assert_ne!(
            database.binding.app_id(),
            database.binding.schema().as_str()
        );
        let write = |db: Database| async move {
            db.collection("records")?
                .insert(value!({"id":"refused", "label":"private"}))
                .await
        };
        let result = if in_transaction {
            database.transaction(write).await
        } else {
            write(database).await
        };
        let error = result
            .expect_err("a different app identity must not bypass the schema's protection floor");
        assert!(
            matches!(
                error,
                DbError::Configuration {
                    code: "protection_removed_from_descriptor",
                    ..
                }
            ),
            "{error:?}"
        );
    }
    let Output::Rows { rows, .. } = owner
        .database
        .collection("records")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert!(rows.is_empty(), "a refused write must leave no plaintext");
    owner.close().await;
}
