use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({"balance":{"type":"number"},"items":{"type":"array","items":"json"},"value":{"type":"array","items":"json"}})
}

#[compio::test]
async fn sqlite_arithmetic_updates_are_atomic() {
    let owner = CollectionFixture::sqlite("accounts", fields()).await;
    exercise_arithmetic(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_arithmetic_updates_are_atomic() {
    let owner = CollectionFixture::postgres("accounts", fields()).await;
    exercise_arithmetic(&owner.database).await;
    owner.close().await;
}

async fn exercise_arithmetic(db: &Database) {
    let accounts = db.collection("accounts").unwrap();
    accounts.insert(value!({"balance":10})).await.unwrap();
    for (patch, expected) in [
        (value!({"balance":{"$inc":2.5}}), 12.5),
        (value!({"balance":{"$dec":0.5}}), 12.0),
        (value!({"balance":{"$mul":2}}), 24.0),
    ] {
        let Output::Rows { rows, .. } = accounts.update(value!({}), patch).await.unwrap() else {
            panic!("update must return rows")
        };
        assert_eq!(rows[0]["balance"].as_f64(), Some(expected));
    }
    let result: Result<(), DbError> = db
        .transaction(|tx| async move {
            tx.collection("accounts")?
                .update(value!({}), value!({"balance":{"$inc":1}}))
                .await?;
            Err(DbError::validation(
                "test_rollback",
                "rollback the arithmetic update",
            ))
        })
        .await;
    assert!(result.is_err());
    let Output::Rows { rows, .. } = accounts.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert_eq!(rows[0]["balance"].as_f64(), Some(24.0));
}

#[compio::test]
async fn sqlite_array_updates_preserve_json_elements() {
    let owner = CollectionFixture::sqlite("accounts", fields()).await;
    exercise_arrays(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_array_updates_preserve_json_elements() {
    let owner = CollectionFixture::postgres("accounts", fields()).await;
    exercise_arrays(&owner.database).await;
    owner.close().await;
}

async fn exercise_arrays(db: &Database) {
    let accounts = db.collection("accounts").unwrap();
    accounts
        .insert(value!({"balance":0,"items":[1,"1",true,{"a":1,"b":2}]}))
        .await
        .unwrap();
    for (operation, expected) in [
        (
            value!({"$push":false}),
            value!([1,"1",true,{"a":1,"b":2},false]),
        ),
        (
            value!({"$push":null}),
            value!([1,"1",true,{"a":1,"b":2},false,null]),
        ),
        (
            value!({"$push":[2,3]}),
            value!([1,"1",true,{"a":1,"b":2},false,null,[2,3]]),
        ),
        (
            value!({"$addToSet":{"b":2,"a":1}}),
            value!([1,"1",true,{"a":1,"b":2},false,null,[2,3]]),
        ),
        (
            value!({"$addToSet":{"a":1}}),
            value!([1,"1",true,{"a":1,"b":2},false,null,[2,3],{"a":1}]),
        ),
        (
            value!({"$pull":1}),
            value!(["1",true,{"a":1,"b":2},false,null,[2,3],{"a":1}]),
        ),
        (
            value!({"$pull":null}),
            value!(["1",true,{"a":1,"b":2},false,[2,3],{"a":1}]),
        ),
        (
            value!({"$push":true}),
            value!(["1",true,{"a":1,"b":2},false,[2,3],{"a":1},true]),
        ),
        (
            value!({"$pull":true}),
            value!(["1",{"a":1,"b":2},false,[2,3],{"a":1}]),
        ),
    ] {
        let Output::Rows { rows, .. } = accounts
            .update(value!({}), value!({"items":operation}))
            .await
            .unwrap()
        else {
            panic!("update must return rows")
        };
        assert_eq!(rows[0]["items"], expected);
    }
    let before = accounts.find(value!({}), value!({})).await.unwrap();
    let rollback: Result<(), DbError> = db
        .transaction(|tx| async move {
            let accounts = tx.collection("accounts")?;
            accounts
                .update(value!({}), value!({"items":{"$push":{"rolled_back":true}}}))
                .await?;
            accounts
                .update(value!({}), value!({"items":{"$pull":{"rolled_back":true}}}))
                .await?;
            accounts
                .update(
                    value!({}),
                    value!({"items":{"$addToSet":{"rolled_back":true}}}),
                )
                .await?;
            Err(DbError::validation(
                "test_rollback",
                "rollback the array updates",
            ))
        })
        .await;
    assert!(rollback.is_err());
    let Output::Rows { rows: before, .. } = before else {
        panic!("find must return rows")
    };
    let Output::Rows { rows: after, .. } = accounts.find(value!({}), value!({})).await.unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(
        before, after,
        "the transaction lane must roll back every array mutation"
    );

    // Exercise a column name also used by SQLite's JSON table functions.
    accounts
        .execute(Operation::InsertMany {
            documents: value!([
                {"balance":100,"value":[18446744073709551615_u64,18446744073709551614_u64]},
                {"balance":100,"value":[18446744073709551615_u64,18446744073709551614_u64]}
            ]),
        })
        .await
        .unwrap();
    accounts
        .execute(Operation::Update {
            filter: value!({"balance":100}),
            patch: value!({"value":{"$pull":18446744073709551615_u64}}),
            many: true,
        })
        .await
        .unwrap();
    accounts
        .execute(Operation::Update {
            filter: value!({"balance":100}),
            patch: value!({"value":{"$addToSet":18446744073709551615_u64}}),
            many: true,
        })
        .await
        .unwrap();
    let Output::Rows { rows, .. } = accounts
        .find(value!({"balance":100}), value!({}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(
            row["value"],
            value!([18446744073709551614_u64, 18446744073709551615_u64])
        );
    }

    for initial in [value!([]), value!(null)] {
        accounts
            .update(
                value!({"balance":0}),
                value!({"items":{"$set":initial.clone()}}),
            )
            .await
            .unwrap();
        let Output::Rows { rows, .. } = accounts
            .update(value!({"balance":0}), value!({"items":{"$pull":null}}))
            .await
            .unwrap()
        else {
            panic!("update must return rows")
        };
        assert_eq!(rows[0]["items"], initial);
    }
    for operation in [value!({"$push":1}), value!({"$addToSet":1})] {
        let Output::Rows { rows, .. } = accounts
            .update(value!({"balance":0}), value!({"items":operation}))
            .await
            .unwrap()
        else {
            panic!("update must return rows")
        };
        assert!(rows[0]["items"].is_null());
    }
}
