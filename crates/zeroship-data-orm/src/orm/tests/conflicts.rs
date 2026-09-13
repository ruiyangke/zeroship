use super::fixtures::CollectionFixture;
use super::*;

fn assert_unique_conflict(error: DbError) {
    match error {
        DbError::UniqueViolation { .. } => {}
        DbError::SchemaRefused {
            code,
            envelope_json,
        } => {
            assert_eq!(code, "unique_violation");
            let envelope: Value = serde_json::from_str(&envelope_json).unwrap();
            assert_eq!(envelope["code"], value!("unique_violation"));
        }
        other => panic!("duplicate identity must be a unique conflict: {other:?}"),
    }
}

async fn exercise(postgres: bool, integer: bool) {
    let (kind, sql_type, original, other) = if integer {
        ("integer", "INTEGER", value!(1), value!(2))
    } else {
        ("string", "TEXT", value!("original"), value!("other"))
    };
    let fields = value!({
        "id":{"type":kind, "primaryKey":true, "required":true},
        "label":{"type":"string", "unique":true, "required":true}
    });
    let columns = format!("id {sql_type} PRIMARY KEY NOT NULL, label TEXT UNIQUE NOT NULL");
    let owner = if postgres {
        CollectionFixture::postgres_from_table_definition("records", fields, &columns).await
    } else {
        CollectionFixture::sqlite_from_table_definition("records", fields, &columns).await
    };
    let records = owner.database.collection("records").unwrap();
    let retained = value!({"id":original.clone(), "label":"retained"});
    records.insert(retained.clone()).await.unwrap();

    for document in [
        value!({"id":original.clone(), "label":"different"}),
        value!({"id":other.clone(), "label":"retained"}),
    ] {
        assert_unique_conflict(records.insert(document.clone()).await.unwrap_err());
        let error = owner
            .database
            .transaction(|transaction| async move {
                transaction.collection("records")?.insert(document).await
            })
            .await
            .unwrap_err();
        assert_unique_conflict(error);
    }

    let Output::Rows { rows, .. } = records.find(value!({}), value!({})).await.unwrap() else {
        panic!("expected rows");
    };
    assert_eq!(rows, vec![retained]);
    records
        .insert(value!({"id":other, "label":"new"}))
        .await
        .unwrap();
    owner.close().await;
}

#[compio::test]
async fn sqlite_text_primary_key_conflicts_are_not_transient() {
    exercise(false, false).await;
}

#[compio::test]
async fn sqlite_integer_primary_key_conflicts_are_not_transient() {
    exercise(false, true).await;
}

#[compio::test]
async fn postgres_text_primary_key_conflicts_are_not_transient() {
    exercise(true, false).await;
}

#[compio::test]
async fn postgres_integer_primary_key_conflicts_are_not_transient() {
    exercise(true, true).await;
}
