use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({"balance":{"type":"number"},"units":{"type":"integer"},"payload":{"type":"json"}})
}

#[compio::test]
async fn sqlite_arithmetic_refuses_invalid_operands_before_mutation() {
    let owner = CollectionFixture::sqlite("accounts", fields()).await;
    exercise_arithmetic_validation(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_arithmetic_refuses_invalid_operands_before_mutation() {
    let owner = CollectionFixture::postgres("accounts", fields()).await;
    exercise_arithmetic_validation(&owner.database).await;
    owner.close().await;
}

async fn exercise_arithmetic_validation(db: &Database) {
    let accounts = db.collection("accounts").unwrap();
    accounts
        .insert(value!({"balance":10,"units":10}))
        .await
        .unwrap();
    let before = accounts.find(value!({}), value!({})).await.unwrap();
    for operand in [
        value!("2"),
        value!("private_not_a_number"),
        value!(true),
        value!(null),
        value!([2]),
        value!({"amount":2}),
        Value::Bytes(vec![2]),
        Value::Timestamp(2),
        Value::Json("2".into()),
        Value::Decimal("private_not_a_number".into()),
    ] {
        for operation in ["$inc", "$dec", "$mul"] {
            let patch = Value::Object(
                [(
                    "balance".into(),
                    Value::Object([(operation.into(), operand.clone())].into()),
                )]
                .into(),
            );
            let error = accounts.update(value!({}), patch).await.unwrap_err();
            assert!(
                matches!(
                    &error,
                    DbError::ValidationFailed {
                        code: "invalid_arithmetic_operand",
                        ..
                    }
                ),
                "{error:?}"
            );
            assert!(!error.message_str().contains("private_not_a_number"));
        }
    }
    for operand in [value!(1.5), Value::Decimal("1.5".into())] {
        for operation in ["$inc", "$dec", "$mul"] {
            let error = accounts
                .update(
                    value!({}),
                    Value::Object(
                        [(
                            "units".into(),
                            Value::Object([(operation.into(), operand.clone())].into()),
                        )]
                        .into(),
                    ),
                )
                .await
                .unwrap_err();
            assert!(error.message_str().contains("numeric column and operand"));
        }
    }
    let Output::Rows { rows: before, .. } = before else {
        panic!("find must return rows")
    };
    let Output::Rows { rows: after, .. } = accounts.find(value!({}), value!({})).await.unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(
        before, after,
        "refused updates must not apply descriptor assignments"
    );

    db.transaction(|tx| async move {
        let accounts = tx.collection("accounts")?;
        let error = accounts
            .update(value!({}), value!({"balance":{"$inc":"invalid"}}))
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                DbError::ValidationFailed {
                    code: "invalid_arithmetic_operand",
                    ..
                }
            ),
            "{error:?}"
        );
        let mut patch = value!({"balance":{"$inc":0}});
        patch["balance"]["$inc"] = Value::Decimal("1.5".into());
        accounts.update(value!({}), patch).await?;
        Ok::<_, DbError>(())
    })
    .await
    .unwrap();
    let Output::Rows { rows, .. } = accounts.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert_eq!(rows[0]["balance"].as_f64(), Some(11.5));
}

#[compio::test]
async fn sqlite_update_grammar_refuses_conflicts_and_preserves_literal_json() {
    let owner = CollectionFixture::sqlite("accounts", fields()).await;
    exercise_update_grammar(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_update_grammar_refuses_conflicts_and_preserves_literal_json() {
    let owner = CollectionFixture::postgres("accounts", fields()).await;
    exercise_update_grammar(&owner.database).await;
    owner.close().await;
}

async fn exercise_update_grammar(db: &Database) {
    let accounts = db.collection("accounts").unwrap();
    accounts.insert(value!({"balance":10})).await.unwrap();
    for patch in [
        value!({"balance":{"$inc":1,"$mul":2}}),
        value!({"balance":{"$inc":1,"ignored":2}}),
        value!({"$set":{"balance":1},"balance":2}),
        value!({"$inc":{"balance":1},"$mul":{"balance":2}}),
        value!({"$unknown":{"balance":1}}),
        value!({"$inc":[1]}),
    ] {
        for many in [false, true] {
            let error = accounts
                .execute(Operation::Update {
                    filter: value!({}),
                    patch: patch.clone(),
                    many,
                })
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::ValidationFailed {
                        code: "invalid_update",
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }
    let literal = value!({"$inc":2,"description":"literal JSON"});
    let Output::Rows { rows, .. } = accounts
        .update(
            value!({}),
            value!({"$set":{"payload":literal.clone()},"$inc":{"balance":1}}),
        )
        .await
        .unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["payload"], literal);
    assert_eq!(rows[0]["balance"].as_f64(), Some(11.0));
    assert_eq!(rows[0]["version"].as_i64(), Some(2));
    let Output::Rows { rows, .. } = accounts
        .update(
            value!({}),
            value!({"payload":{"$set":{"$mul":3}},"balance":{"$mul":2}}),
        )
        .await
        .unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["payload"], value!({"$mul":3}));
    assert_eq!(rows[0]["balance"].as_f64(), Some(22.0));
}

#[compio::test]
async fn sqlite_patches_emptied_by_write_assignments_are_refused() {
    let owner = CollectionFixture::sqlite("accounts", fields()).await;
    exercise_patches_emptied_by_write_assignments(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_patches_emptied_by_write_assignments_are_refused() {
    let owner = CollectionFixture::postgres("accounts", fields()).await;
    exercise_patches_emptied_by_write_assignments(&owner.database).await;
    owner.close().await;
}

async fn stored_rows(accounts: &Collection) -> Vec<Value> {
    let Output::Rows { rows, .. } = accounts.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert_eq!(rows.len(), 1, "the fixture holds one account");
    rows
}

/// The fixture descriptor reassigns `version` and `updated_at` on every write,
/// so a dynamic patch naming only those fields asks for no caller write at all.
async fn exercise_patches_emptied_by_write_assignments(db: &Database) {
    let accounts = db.collection("accounts").unwrap();
    accounts.insert(value!({"balance":10})).await.unwrap();
    let before = stored_rows(&accounts).await;
    assert_eq!(before[0]["version"].as_i64(), Some(1));
    assert!(!before[0]["updated_at"].is_null());
    let stamp = Value::Timestamp(0);
    let assigned_only = [
        value!({"version":99}),
        value!({"$set":{"version":99}}),
        value!({"version":{"$inc":5}}),
        Value::Object([("updated_at".into(), stamp.clone())].into()),
        value!({"$set":{}}),
        {
            let mut patch = value!({"$set":{"version":99}});
            patch["$set"]["updated_at"] = stamp.clone();
            patch
        },
    ];
    for patch in assigned_only {
        for many in [false, true] {
            let error = accounts
                .execute(Operation::Update {
                    filter: value!({}),
                    patch: patch.clone(),
                    many,
                })
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::ValidationFailed {
                        code: "invalid_update",
                        ..
                    }
                ),
                "{patch:?} many={many}: {error:?}"
            );
            let after = stored_rows(&accounts).await;
            assert_eq!(after[0]["version"], before[0]["version"], "{patch:?}");
            assert_eq!(after[0]["updated_at"], before[0]["updated_at"], "{patch:?}");
            assert_eq!(
                after, before,
                "refused updates must not apply descriptor assignments"
            );
        }
    }

    let Output::Rows { rows, .. } = accounts
        .update(value!({}), value!({"balance":11,"version":99}))
        .await
        .unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["balance"].as_f64(), Some(11.0));
    assert_eq!(
        rows[0]["version"].as_i64(),
        Some(2),
        "a remaining caller field writes and the supplied version is replaced"
    );
    let Output::Count(count) = accounts
        .execute(Operation::Update {
            filter: value!({}),
            patch: Value::Object(
                [("balance".into(), value!(12)), ("updated_at".into(), stamp)].into(),
            ),
            many: true,
        })
        .await
        .unwrap()
    else {
        panic!("updateMany must return a count")
    };
    assert_eq!(count, 1);
    let after = stored_rows(&accounts).await;
    assert_eq!(after[0]["balance"].as_f64(), Some(12.0));
    assert_eq!(after[0]["version"].as_i64(), Some(3));
}
