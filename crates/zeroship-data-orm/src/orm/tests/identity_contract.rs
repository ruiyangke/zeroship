use super::fixtures::CollectionFixture;
use super::*;

async fn fixture(postgres: bool) -> CollectionFixture {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("identity_fixture", &"1".repeat(64))
        .unwrap();
    let fields = value!({
        "label":{"type":"string","unique":true},
        "note":{"type":"string"},
        "secret":{"type":"string","encrypted":true}
    });
    let source = ProjectKeySource::supplied(keys.clone());
    let owner = if postgres {
        CollectionFixture::postgres_with_keys("records", fields, source).await
    } else {
        CollectionFixture::sqlite_with_keys("records", fields, source).await
    };
    keys.bind_app(owner.database.binding.app_id(), "identity_fixture")
        .unwrap();
    owner
}

fn manual_database(owner: &CollectionFixture) -> Database {
    let mut fields = owner.database.context.with(|| {
        crate::descriptor::collection_schema(&owner.database.binding, "records")
            .unwrap()
            .as_ref()
            .clone()
    });
    fields["id"].as_object_mut().unwrap().shift_remove("assign");
    fields["id"]["writable"] = Value::Bool(true);
    Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        vec![("records".into(), fields)],
    )
    .unwrap()
}

fn row(output: Output) -> Value {
    let Output::Rows { mut rows, .. } = output else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

async fn manual_identity(postgres: bool) {
    let owner = fixture(postgres).await;
    let db = manual_database(&owner);
    let records = db.collection("records").unwrap();
    records
        .insert(value!({"id":"original", "label":"row", "secret":"value"}))
        .await
        .unwrap();
    for many in [false, true] {
        for patch in [
            value!({"id":"replacement"}),
            value!({"$set":{"id":"replacement"}}),
        ] {
            let error = records
                .execute(Operation::Update {
                    filter: value!({"id":"original"}),
                    patch,
                    many,
                })
                .await
                .expect_err("identity cannot be updated");
            assert!(
                matches!(
                    error,
                    DbError::ValidationFailed {
                        code: "immutable_primary_key",
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }
    for document in [
        value!({"id":"replacement", "label":"row", "note":"changed"}),
        value!({"id":"replacement", "label":"row", "secret":"updated"}),
    ] {
        let upserted = row(records
            .execute(Operation::Upsert {
                document,
                conflict_fields: value!(["label"]),
            })
            .await
            .unwrap());
        assert_eq!(upserted["id"], value!("original"));
    }
    let stored = row(records
        .find(value!({"id":"original"}), value!({}))
        .await
        .unwrap());
    assert_eq!(stored["secret"], value!("updated"));
    assert_eq!(stored["note"], value!("changed"));
    owner.close().await;
}

async fn cas_identity(postgres: bool) {
    let owner = fixture(postgres).await;
    let records = owner.database.collection("records").unwrap();
    let first = row(records.insert(value!({"label":"a"})).await.unwrap());
    let second = row(records.insert(value!({"label":"b"})).await.unwrap());
    for id in [
        value!({"$ne":"missing"}),
        value!({"$gt":""}),
        value!({"$in":[first["id"].clone(),second["id"].clone()]}),
        Value::Null,
        value!({}),
        value!({"$eq":Value::Null}),
    ] {
        for many in [false, true] {
            let result = records
                .execute(Operation::Update {
                    filter: value!({"id":id.clone(), "version":1}),
                    patch: value!({"note":"invalid"}),
                    many,
                })
                .await;
            assert!(
                matches!(
                    result,
                    Err(DbError::ValidationFailed {
                        code: "multi_row_concurrency_filter_unsupported",
                        ..
                    })
                ),
                "{id:?}: {result:?}"
            );
        }
    }
    for (id, version) in [
        (first["id"].clone(), 1),
        (value!({"$eq":first["id"].clone()}), 2),
    ] {
        let result = records
            .execute(Operation::Update {
                filter: value!({"id":id,"version":version}),
                patch: value!({"note":"valid"}),
                many: true,
            })
            .await
            .unwrap();
        assert!(matches!(result, Output::Count(1)));
    }
    assert_eq!(
        row(records
            .find(value!({"id":second["id"].clone()}), value!({}))
            .await
            .unwrap())["version"],
        value!(1)
    );
    owner.close().await;
}

#[compio::test]
async fn sqlite_manual_identity_is_immutable() {
    manual_identity(false).await;
}
#[compio::test]
async fn postgres_manual_identity_is_immutable() {
    manual_identity(true).await;
}
#[compio::test]
async fn sqlite_cas_requires_one_id() {
    cas_identity(false).await;
}
#[compio::test]
async fn postgres_cas_requires_one_id() {
    cas_identity(true).await;
}
