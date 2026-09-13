use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "key":{"type":"string", "unique":true},
        "profile":{"type":"object", "shape":{
            "name":{"type":"string"},
            "visits":{"type":"integer"},
            "balance":{"type":"number"}
        }}
    })
}

#[compio::test]
async fn sqlite_nested_scalars_obey_their_declared_types() {
    let owner = CollectionFixture::sqlite("profiles", fields()).await;
    exercise_scalar_types(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_nested_scalars_obey_their_declared_types() {
    let owner = CollectionFixture::postgres("profiles", fields()).await;
    exercise_scalar_types(&owner.database).await;
    owner.close().await;
}

async fn exercise_scalar_types(database: &Database) {
    let profiles = database.collection("profiles").unwrap();
    let profile = value!({"name":"Ada", "visits":2, "balance":1.5});
    let original = value!({"key":"original", "profile":profile});
    profiles.insert(original.clone()).await.unwrap();
    for invalid in [
        value!({"name":false}),
        value!({"visits":"private-invalid-number"}),
        value!({"visits":1.5}),
        value!({"balance":true}),
    ] {
        for encoded in [false, true] {
            let payload = if encoded {
                Value::Json(invalid.to_string())
            } else {
                invalid.clone()
            };
            for operation in [
                Operation::Insert {
                    document: value!({"key":"invalid", "profile":payload.clone()}),
                },
                Operation::InsertMany {
                    documents: value!([
                        {"key":"batch-valid", "profile":profile.clone()},
                        {"key":"invalid", "profile":payload.clone()}
                    ]),
                },
                Operation::Upsert {
                    document: value!({"key":"original", "profile":payload.clone()}),
                    conflict_fields: value!(["key"]),
                },
            ] {
                let error = profiles.execute(operation).await.unwrap_err();
                assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
                assert!(!error.to_string().contains("private-invalid-number"));
            }
            let error = profiles
                .update(value!({"key":"original"}), value!({"profile":payload}))
                .await
                .unwrap_err();
            assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
        }
    }
    let Output::Rows { rows, .. } = profiles.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["profile"], original["profile"]);
}
