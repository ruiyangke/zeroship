use super::fixtures::CollectionFixture;
use super::*;
use crate::sql::{AggregateFunc, AggregateRef, Operand};

async fn exercise(postgres: bool) {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("aggregate_fixture", &"4".repeat(64))
        .unwrap();
    let fields = value!({
        "visible":{"type":"string"},
        "hidden":{"type":"string", "readable":false, "projectable":false},
        "masked":{"type":"string", "mask":{"kind":"last4", "classification":"pii"},
            "storage":{"valueColumn":"masked", "rawColumn":"__zs_raw__masked"}},
        "encrypted":{"type":"string", "encrypted":true},
    });
    let key_source = ProjectKeySource::supplied(keys.clone());
    let owner = if postgres {
        CollectionFixture::postgres_with_keys("records", fields, key_source).await
    } else {
        CollectionFixture::sqlite_with_keys("records", fields, key_source).await
    };
    let db = &owner.database;
    keys.bind_app(db.binding.app_id(), "aggregate_fixture")
        .unwrap();
    db.collection("records").unwrap().insert(value!({
        "visible":"public", "hidden":"private", "masked":"12345678", "encrypted":"ciphertext input"
    })).await.unwrap();
    let source = ReadSource::new("records", "r");
    let query = |expression| {
        let mut query = ReadQuery::new(source.clone());
        query.projection = vec![ReadProjection::Scalar {
            output: "result".into(),
            expression,
        }];
        query
    };
    let aggregate = |field, function| {
        Operand::Aggregate(
            AggregateRef::over_path(function, source.column(field).unwrap(), false).unwrap(),
        )
    };
    let Output::Rows { rows, .. } = db
        .read(query(aggregate("visible", AggregateFunc::Count)))
        .await
        .unwrap()
    else {
        panic!("expected count row");
    };
    assert_eq!(rows[0]["result"], value!(1));

    for field in ["hidden", "masked", "encrypted"] {
        for function in [AggregateFunc::Count, AggregateFunc::Min, AggregateFunc::Max] {
            let result = db.read(query(aggregate(field, function))).await;
            assert!(
                matches!(
                    result,
                    Err(DbError::ValidationFailed {
                        code: "invalid_read",
                        ..
                    })
                ),
                "{field} {function:?}: {result:?}"
            );
        }
        let mut having = query(aggregate("visible", AggregateFunc::Count));
        having.having = crate::sql::Predicate::IsNull {
            operand: aggregate(field, AggregateFunc::Count),
            negated: true,
        };
        assert!(matches!(
            db.read(having).await,
            Err(DbError::ValidationFailed {
                code: "invalid_read",
                ..
            })
        ));
    }

    let Output::Rows { rows, has_masked } = db
        .read(query(Operand::Path(source.column("masked").unwrap())))
        .await
        .unwrap()
    else {
        panic!("expected protected projection");
    };
    assert!(has_masked);
    assert_eq!(rows[0]["result"]["_meta"]["column"], value!("masked"));
    let Output::Rows { rows, .. } = db
        .read(query(Operand::Path(source.column("encrypted").unwrap())))
        .await
        .unwrap()
    else {
        panic!("expected decrypted projection");
    };
    assert_eq!(rows[0]["result"], value!("ciphertext input"));
    owner.close().await;
}

#[compio::test]
async fn sqlite_aggregate_projections_enforce_column_protection() {
    exercise(false).await;
}

#[compio::test]
async fn postgres_aggregate_projections_enforce_column_protection() {
    exercise(true).await;
}
