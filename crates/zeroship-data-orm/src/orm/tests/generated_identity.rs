use super::fixtures::CollectionFixture;
use super::*;

async fn fixture(postgres: bool, key: &str) -> CollectionFixture {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("generated_identity", &"3".repeat(64))
        .unwrap();
    let source = ProjectKeySource::supplied(keys.clone());
    let mut owner = if postgres {
        CollectionFixture::postgres_with_keys(
            "records",
            value!({"label":{"type":"string"}}),
            source,
        )
        .await
    } else {
        CollectionFixture::sqlite_with_keys("records", value!({"label":{"type":"string"}}), source)
            .await
    };
    keys.bind_app(owner.database.binding.app_id(), "generated_identity")
        .unwrap();
    let mut migration: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/identity-migration.json"
    ))
    .unwrap();
    migration["ops"][0]["columns"][0]["identity"]["always"] = serde_json::json!(postgres);
    migration["ops"][0]["columns"][0]["name"] = serde_json::json!(key);
    migration["ops"][0]["primaryKey"] = serde_json::json!([key]);
    owner
        .replace_from_migration_with_policy("records", &migration.to_string(), zeroship_migrate_server::policy::PLATFORM_CEILING_TOML)
        .await;
    owner
}

fn rows(output: Output) -> Vec<Value> {
    let Output::Rows { rows, .. } = output else {
        panic!("expected rows")
    };
    rows
}

async fn generated_insert(postgres: bool, batch: bool, key: &str) {
    let owner = fixture(postgres, key).await;
    let records = owner.database.collection("records").unwrap();
    let plain = rows(records.insert(value!({"label":"plain"})).await.unwrap()).remove(0);
    assert!(plain[key].as_i64().is_some());
    let inserted = if batch {
        rows(records.execute(Operation::InsertMany { documents:value!([
            {"label":"encrypted", "secret":"private"}, {"label":"null", "secret":null}, {"label":"second", "secret":"other"}
        ]) }).await.unwrap())
    } else {
        rows(
            records
                .insert(value!({"label":"encrypted", "secret":"private"}))
                .await
                .unwrap(),
        )
    };
    let all = rows(records.find(value!({}), value!({})).await.unwrap());
    for row in &inserted {
        assert!(row[key].as_i64().unwrap() > plain[key].as_i64().unwrap());
        assert_eq!(
            all.iter().find(|stored| stored[key] == row[key]),
            Some(row)
        );
    }
    assert_eq!(inserted[0]["secret"], value!("private"));
    if batch {
        assert_eq!(inserted[2]["secret"], value!("other"));
    }
    let last = inserted
        .iter()
        .map(|row| row[key].as_i64().unwrap())
        .max()
        .unwrap();
    records
        .execute(Operation::Purge {
            filter: Value::Object([(key.to_owned(), value!(last))].into()),
            many: false,
        })
        .await
        .unwrap();
    let next = rows(
        records
            .insert(value!({"label":"after_delete", "secret":"retained"}))
            .await
            .unwrap(),
    )
    .remove(0);
    assert!(
        next[key].as_i64().unwrap() > last,
        "deleted identities cannot be reused"
    );

    let concurrent = futures::future::join_all((0..8).map(|index| {
        records.insert(
            value!({"label":format!("concurrent_{index}"), "secret":format!("private_{index}")}),
        )
    }))
    .await;
    let mut identities = std::collections::BTreeSet::new();
    for (index, output) in concurrent.into_iter().enumerate() {
        let row = rows(output.unwrap()).remove(0);
        assert!(identities.insert(row[key].as_i64().unwrap()));
        assert_eq!(row["secret"], value!(format!("private_{index}")));
    }
    owner.close().await;
}

async fn generated_upsert(postgres: bool, key: &str) {
    let owner = fixture(postgres, key).await;
    let records = owner.database.collection("records").unwrap();
    let mut identity = Value::Null;
    for secret in ["private", "updated"] {
        let row = rows(
            records
                .execute(Operation::Upsert {
                    document: value!({"label":"unique", "secret":secret}),
                    conflict_fields: value!(["label"]),
                })
                .await
                .unwrap(),
        )
        .remove(0);
        assert_eq!(row["secret"], value!(secret));
        if identity.is_null() {
            identity = row[key].clone();
        }
        assert_eq!(row[key], identity);
    }
    let failure: Result<(), DbError> = owner
        .database
        .transaction(|tx| async move {
            tx.collection("records")?
                .insert(value!({"label":"rolled_back", "secret":"discard"}))
                .await?;
            Err(DbError::internal("rollback the fixture write"))
        })
        .await;
    assert!(failure.is_err());
    assert_eq!(
        rows(records.find(value!({}), value!({})).await.unwrap()).len(),
        1
    );
    let committed = owner
        .database
        .transaction(|tx| async move {
            let records = tx.collection("records")?;
            let failed = records.execute(Operation::InsertMany { documents:value!([
                {"label":"must_rollback", "secret":"discard"}, {"label":"unique", "secret":"duplicate"}
            ]) }).await;
            assert!(failed.is_err());
            assert!(rows(records.find(value!({"label":"must_rollback"}), value!({})).await?).is_empty());
            tx.collection("records")?
                .insert(value!({"label":"committed", "secret":"keep"}))
                .await
        })
        .await
        .unwrap();
    assert_eq!(rows(committed)[0]["secret"], value!("keep"));
    owner.close().await;
}

#[compio::test]
async fn sqlite_generated_encrypted_insert() {
    generated_insert(false, false, "id").await;
}
#[compio::test]
async fn postgres_generated_encrypted_insert() {
    generated_insert(true, false, "id").await;
}
#[compio::test]
async fn sqlite_generated_encrypted_batch() {
    generated_insert(false, true, "id").await;
}
#[compio::test]
async fn postgres_generated_encrypted_batch() {
    generated_insert(true, true, "id").await;
}
#[compio::test]
async fn sqlite_generated_encrypted_upsert_and_rollback() {
    generated_upsert(false, "id").await;
}
#[compio::test]
async fn postgres_generated_encrypted_upsert_and_rollback() {
    generated_upsert(true, "id").await;
}

#[compio::test]
async fn sqlite_named_generated_encrypted_identity() {
    generated_insert(false, true, "record_key").await;
    generated_upsert(false, "record_key").await;
}
#[compio::test]
async fn postgres_named_generated_encrypted_identity() {
    generated_insert(true, true, "record_key").await;
    generated_upsert(true, "record_key").await;
}
