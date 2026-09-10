use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "label":{"type":"string"},
        "active":{"type":"boolean"},
        "payload":{"type":"json"},
        "balance":{"type":"number"},
        "items":{"type":"array","items":"json"}
    })
}

#[compio::test]
async fn sqlite_updates_refuse_operators_for_incompatible_field_types() {
    let owner = CollectionFixture::sqlite("records", fields()).await;
    exercise_field_types(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_updates_refuse_operators_for_incompatible_field_types() {
    let owner = CollectionFixture::postgres("records", fields()).await;
    exercise_field_types(&owner.database).await;
    owner.close().await;
}

async fn exercise_field_types(db: &Database) {
    let records = db.collection("records").unwrap();
    records
        .insert(value!({"label":"original","active":true,"payload":[],"balance":10,"items":[]}))
        .await
        .unwrap();
    let Output::Rows { rows: before, .. } = records.find(value!({}), value!({})).await.unwrap()
    else {
        panic!("find must return rows")
    };
    for (fields, operators) in [
        (
            &["label", "active", "payload", "items"][..],
            &["$inc", "$dec", "$mul"][..],
        ),
        (
            &["label", "active", "payload", "balance"][..],
            &["$push", "$pull", "$addToSet"][..],
        ),
    ] {
        for field in fields {
            for operator in operators {
                for many in [false, true] {
                    for label in ["original", "missing"] {
                        let patch = Value::Object(
                            [(
                                (*field).into(),
                                Value::Object([((*operator).into(), Value::from(2))].into()),
                            )]
                            .into(),
                        );
                        let error = records
                            .execute(Operation::Update {
                                filter: value!({"label":label}),
                                patch,
                                many,
                            })
                            .await
                            .unwrap_err();
                        assert!(
                            matches!(
                                error,
                                DbError::ValidationFailed {
                                    code: "invalid_update_operation",
                                    ..
                                }
                            ),
                            "{field} {operator}: {error:?}"
                        );
                    }
                }
            }
        }
    }
    let Output::Rows { rows: after, .. } = records.find(value!({}), value!({})).await.unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(
        before, after,
        "invalid updates must leave the row unchanged"
    );
}
