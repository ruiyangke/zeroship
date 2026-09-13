use super::fixtures::CollectionFixture;
use super::*;

fn decimal(value: &str) -> Value {
    Value::Decimal(value.into())
}

fn document(value: &str) -> Value {
    Value::Object([("amount".into(), decimal(value))].into())
}

fn increment(value: &str) -> Value {
    arithmetic("$inc", value)
}

fn arithmetic(operator: &str, value: &str) -> Value {
    Value::Object(
        [(
            "amount".into(),
            Value::Object([(operator.into(), decimal(value))].into()),
        )]
        .into(),
    )
}

fn membership(values: &[&str]) -> Value {
    Value::Object(
        [(
            "amount".into(),
            Value::Object(
                [(
                    "$in".into(),
                    Value::Array(values.iter().map(|value| decimal(value)).collect()),
                )]
                .into(),
            ),
        )]
        .into(),
    )
}

async fn exercise(owner: &CollectionFixture) {
    let records = owner.database.collection("records").unwrap();
    let Output::Rows { rows, .. } = records
        .insert(document("9007199254740993.00"))
        .await
        .unwrap()
    else {
        panic!("insert must return rows")
    };
    assert_eq!(rows[0]["amount"], decimal("9007199254740993.00"));

    let Output::Rows { rows, .. } = records
        .find(document("9007199254740993.000"), value!({}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(rows.len(), 1);

    let Output::Rows { rows, .. } = records.update(value!({}), increment("0.01")).await.unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["amount"], decimal("9007199254740993.01"));

    let Output::Rows { rows, .. } = records.insert(document("1.005")).await.unwrap() else {
        panic!("insert must return rows")
    };
    assert_eq!(rows[0]["amount"], decimal("1.01"));

    let Output::Rows { rows, .. } = records
        .find(membership(&["0", "1.010"]), value!({}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(rows.len(), 1);

    let Output::Rows { rows, .. } = records
        .update(document("1.010"), arithmetic("$mul", "2"))
        .await
        .unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["amount"], decimal("2.02"));

    let Output::Rows { rows, .. } = records
        .update(document("2.020"), arithmetic("$dec", "0.02"))
        .await
        .unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["amount"], decimal("2.00"));

    assert!(records
        .insert(document("9999999999999999999999999999.995"))
        .await
        .is_err());
}

async fn exercise_v8_shape(owner: &CollectionFixture) {
    let records = owner.database.collection("records").unwrap();
    let Output::Rows { rows, .. } = records
        .insert(value!({"amount":"9007199254740993.00"}))
        .await
        .unwrap()
    else {
        panic!("insert must return rows")
    };
    assert_eq!(rows[0]["amount"], decimal("9007199254740993.00"));
    let Output::Rows { rows, .. } = records
        .update(
            value!({"amount":"9007199254740993.000"}),
            value!({"amount":{"$inc":"0.01"}}),
        )
        .await
        .unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["amount"], decimal("9007199254740993.01"));
}

#[compio::test]
async fn sqlite_decimal_values_remain_exact() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({"amount":{"type":"number","precision":30,"scale":2}}),
    )
    .await;
    exercise(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn sqlite_decimal_strings_use_the_v8_contract() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({"amount":{"type":"number","precision":30,"scale":2}}),
    )
    .await;
    exercise_v8_shape(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_decimal_values_remain_exact() {
    let owner = CollectionFixture::postgres(
        "records",
        value!({"amount":{"type":"number","precision":30,"scale":2}}),
    )
    .await;
    exercise(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_decimal_strings_use_the_v8_contract() {
    let owner = CollectionFixture::postgres(
        "records",
        value!({"amount":{"type":"number","precision":30,"scale":2}}),
    )
    .await;
    exercise_v8_shape(&owner).await;
    owner.close().await;
}
