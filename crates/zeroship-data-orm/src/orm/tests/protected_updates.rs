use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "label":{"type":"string"},
        "random":{"type":"number","encrypted":{"keyId":"update_fixture","wraps":"number"}},
        "second_secret":{"type":"number","encrypted":{"keyId":"update_fixture","wraps":"number"}},
        "masked":{"type":"number","mask":{"kind":"full","classification":"spi"}},
        "plain":{"type":"number","mask":{"kind":"none","classification":"spi"}}
    })
}

fn keys() -> LocalKeySource {
    let supplied = Rc::new(crate::encryption::SuppliedRootKeys::new());
    supplied
        .insert_hex("update_fixture", &"1".repeat(64))
        .unwrap();
    LocalKeySource::supplied(supplied)
}

#[compio::test]
async fn sqlite_updates_refuse_operations_on_protected_storage() {
    let owner = CollectionFixture::sqlite_with_keys("records", fields(), keys()).await;
    exercise_protected_updates(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_updates_refuse_operations_on_protected_storage() {
    let owner = CollectionFixture::postgres_with_keys("records", fields(), keys()).await;
    exercise_protected_updates(&owner.database).await;
    owner.close().await;
}

async fn exercise_protected_updates(db: &Database) {
    let records = db.collection("records").unwrap();
    records
        .insert(value!({"label":"original","random":10,"second_secret":20,"masked":30,"plain":40}))
        .await
        .unwrap();
    let Output::Rows { rows: before, .. } = records.find(value!({}), value!({})).await.unwrap()
    else {
        panic!("find must return rows")
    };
    for field in ["random", "second_secret", "masked"] {
        for operator in ["$inc", "$dec", "$mul", "$push", "$pull", "$addToSet"] {
            for many in [false, true] {
                for label in ["missing", "original"] {
                    for mixed in [false, true] {
                        let mut patch = Value::Object(
                            [(
                                field.into(),
                                Value::Object([(operator.into(), Value::from(2))].into()),
                            )]
                            .into(),
                        );
                        if mixed {
                            patch
                                .as_object_mut()
                                .unwrap()
                                .insert("$set".into(), value!({"plain":50}));
                        }
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
                                    code: "protected_update_operation",
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
        "refused operations must preserve protected data"
    );

    db.transaction(|tx| async move {
        let records = tx.collection("records")?;
        let error = records
            .update(value!({}), value!({"random":{"$inc":1}}))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbError::ValidationFailed {
                code: "protected_update_operation",
                ..
            }
        ));
        records
            .update(
                value!({}),
                value!({"$set":{"random":11,"second_secret":21,"masked":31},"plain":{"$inc":2}}),
            )
            .await?;
        Ok(())
    })
    .await
    .unwrap();
    let Output::Rows { rows, .. } = records.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert_eq!(rows[0]["random"].as_f64(), Some(11.0));
    assert_eq!(rows[0]["second_secret"].as_f64(), Some(21.0));
    assert_eq!(rows[0]["plain"].as_f64(), Some(42.0));
    assert_eq!(rows[0]["version"].as_i64(), Some(2));
}
