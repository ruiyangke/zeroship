use super::fixtures::CollectionFixture;
use super::*;

fn row(output: Output) -> Value {
    let Output::Rows { mut rows, .. } = output else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

async fn nullable_conflict(postgres: bool) {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("nullable_upsert", &"4".repeat(64)).unwrap();
    let source = ProjectKeySource::supplied(keys.clone());
    let fields = value!({
        "label":{"type":"string","nullable":true,"unique":true},
        "secret":{"type":"string","encrypted":true}
    });
    let owner = if postgres {
        CollectionFixture::postgres_with_keys("records", fields, source).await
    } else {
        CollectionFixture::sqlite_with_keys("records", fields, source).await
    };
    keys.bind_app(owner.database.binding.app_id(), "nullable_upsert")
        .unwrap();
    let records = owner.database.collection("records").unwrap();

    let error = records
        .execute(Operation::Upsert {
            document: value!({"label":null,"secret":"protected"}),
            conflict_fields: value!(["label"]),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DbError::ValidationFailed { code, .. } if code == "protected_upsert_nullable_conflict"
    ));
    assert_eq!(
        count(records.count(value!({}), value!({})).await.unwrap()),
        0
    );

    records
        .execute(Operation::Upsert {
            document: value!({"label":null}),
            conflict_fields: value!(["label"]),
        })
        .await
        .unwrap();
    records
        .execute(Operation::Upsert {
            document: value!({"label":null}),
            conflict_fields: value!(["label"]),
        })
        .await
        .unwrap();
    assert_eq!(
        count(records.count(value!({}), value!({})).await.unwrap()),
        2
    );
    owner.close().await;
}

#[compio::test]
async fn sqlite_protected_upsert_refuses_a_null_conflict_value() {
    nullable_conflict(false).await;
}

#[compio::test]
async fn postgres_protected_upsert_refuses_a_null_conflict_value() {
    nullable_conflict(true).await;
}

async fn composite_conflict(postgres: bool) {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("composite_upsert", &"5".repeat(64))
        .unwrap();
    let source = ProjectKeySource::supplied(keys.clone());
    let fields = value!({
        "tenant":{"type":"string"},
        "label":{"type":"string"},
        "nonce":{"type":"string","unique":true},
        "secret":{"type":"string","encrypted":true}
    });
    let owner = if postgres {
        CollectionFixture::postgres_with_keys("records", fields, source).await
    } else {
        CollectionFixture::sqlite_with_keys("records", fields, source).await
    };
    keys.bind_app(owner.database.binding.app_id(), "composite_upsert")
        .unwrap();
    owner
        .add_unique_index("records", &["tenant", "label"])
        .await;
    let records = owner.database.collection("records").unwrap();

    let first = row(records
        .execute(Operation::Upsert {
            document: value!({
                "tenant":"tenant-a","label":"same","nonce":"first","secret":"before"
            }),
            conflict_fields: value!(["tenant", "label"]),
        })
        .await
        .unwrap());
    let updated = row(records
        .execute(Operation::Upsert {
            document: value!({
                "tenant":"tenant-a","label":"same","nonce":"second","secret":"after"
            }),
            conflict_fields: value!(["tenant", "label"]),
        })
        .await
        .unwrap());
    assert_eq!(updated["id"], first["id"]);
    assert_eq!(updated["secret"], value!("after"));

    let error = records
        .execute(Operation::Upsert {
            document: value!({
                "tenant":"tenant-b","label":"other","nonce":"second","secret":"rejected"
            }),
            conflict_fields: value!(["tenant", "label"]),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unique") || error.to_string().contains("UNIQUE"));
    assert_eq!(
        count(records.count(value!({}), value!({})).await.unwrap()),
        1
    );
    owner.close().await;
}

#[compio::test]
async fn sqlite_protected_upsert_uses_the_complete_conflict_target() {
    composite_conflict(false).await;
}

#[compio::test]
async fn postgres_protected_upsert_uses_the_complete_conflict_target() {
    composite_conflict(true).await;
}

async fn no_change_upsert(postgres: bool) {
    let owner = if postgres {
        CollectionFixture::postgres("records", value!({"label":{"type":"string","unique":true}}))
            .await
    } else {
        CollectionFixture::sqlite("records", value!({"label":{"type":"string","unique":true}}))
            .await
    };
    let records = owner.database.collection("records").unwrap();
    let first = row(records
        .execute(Operation::Upsert {
            document: value!({"label":"same"}),
            conflict_fields: value!(["label"]),
        })
        .await
        .unwrap());
    let second = row(records
        .execute(Operation::Upsert {
            document: value!({"label":"same"}),
            conflict_fields: value!(["label"]),
        })
        .await
        .unwrap());
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["version"], value!(2));
    assert_eq!(
        count(records.count(value!({}), value!({})).await.unwrap()),
        1
    );
    owner.close().await;
}

#[compio::test]
async fn sqlite_no_change_upsert_returns_the_existing_row() {
    no_change_upsert(false).await;
}

#[compio::test]
async fn postgres_no_change_upsert_returns_the_existing_row() {
    no_change_upsert(true).await;
}
