use super::fixtures::CollectionFixture;
use super::*;

fn rows(output: Output) -> Vec<Value> {
    let Output::Rows { rows, .. } = output else {
        panic!("expected rows")
    };
    rows
}

async fn exercise(postgres: bool) {
    let fields = value!({
        "category":{"type":"string"},
        "views":{"type":"integer"}
    });
    let owner = if postgres {
        CollectionFixture::postgres("records", fields).await
    } else {
        CollectionFixture::sqlite("records", fields).await
    };
    let records = owner.database.collection("records").unwrap();
    records
        .execute(Operation::InsertMany {
            documents: value!([
                {"category":"tech","views":10},
                {"category":"tech","views":20},
                {"category":"food","views":5}
            ]),
        })
        .await
        .unwrap();

    assert!(matches!(
        records
            .count(value!({"category":"tech"}), value!({}))
            .await
            .unwrap(),
        Output::Count(2)
    ));
    let found = rows(
        records
            .find(
                value!({"category":"tech"}),
                value!({"select":["category","views"],"orderBy":{"views":-1},"limit":1}),
            )
            .await
            .unwrap(),
    );
    assert_eq!(found, vec![value!({"category":"tech","views":20})]);

    let distinct = rows(
        records
            .execute(Operation::Distinct {
                field: "category".into(),
                filter: value!({}),
                options: value!({}),
            })
            .await
            .unwrap(),
    );
    assert_eq!(distinct, vec![value!("food"), value!("tech")]);

    let aggregated = rows(
        records
            .execute(Operation::Aggregate {
                pipeline: value!([
                    {"$group":{
                        "by":"category",
                        "records":{"$count":true},
                        "total":{"$sum":"views"},
                        "average":{"$avg":"views"},
                        "lowest":{"$min":"views"},
                        "highest":{"$max":"views"}
                    }},
                    {"$having":{"records":{"$gt":1}}},
                    {"$sort":{"total":-1}},
                    {"$limit":1}
                ]),
                options: value!({}),
            })
            .await
            .unwrap(),
    );
    assert_eq!(aggregated.len(), 1);
    assert_eq!(aggregated[0]["category"], value!("tech"));
    assert_eq!(aggregated[0]["records"], value!(2));
    assert_eq!(aggregated[0]["total"], value!(30));
    assert_eq!(aggregated[0]["average"].as_f64(), Some(15.0));
    assert_eq!(aggregated[0]["lowest"], value!(10));
    assert_eq!(aggregated[0]["highest"], value!(20));
    owner.close().await;
}

#[compio::test]
async fn sqlite_dynamic_reads_use_the_registered_compiler() {
    exercise(false).await;
}

#[compio::test]
async fn postgres_dynamic_reads_use_the_registered_compiler() {
    exercise(true).await;
}

#[compio::test]
async fn dynamic_find_rejects_malformed_options() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({
            "category":{"type":"string"},
            "views":{"type":"integer"}
        }),
    )
    .await;
    let records = owner.database.collection("records").unwrap();

    for (options, expected) in [
        (value!({"limit":"1"}), "limit must be an integer"),
        (value!({"offset":"1"}), "offset must be an integer"),
        (
            value!({"include_deleted":"true"}),
            "include_deleted must be a boolean",
        ),
        (value!({"unmask":"category"}), "unmask must be an array"),
        (
            value!({"unmask":["category", 1]}),
            "unmask entries must be strings",
        ),
        (value!({"unmaskReason":1}), "unmaskReason must be a string"),
        (
            value!({"orderBy":{"views":0}}),
            "orderBy direction must be 1 or -1",
        ),
        (
            value!({"orderBy":[["views", "desc"]]}),
            "orderBy direction must be 1 or -1",
        ),
    ] {
        let error = records.find(value!({}), options).await.unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error}"
        );
    }

    let error = records
        .find(value!({}), value!("invalid options"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("find options must be an object"));
    owner.close().await;
}

#[compio::test]
async fn other_dynamic_reads_reject_malformed_options() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({
            "category":{"type":"string"},
            "views":{"type":"integer"}
        }),
    )
    .await;
    let records = owner.database.collection("records").unwrap();

    let error = records
        .count(value!({}), value!({"include_deleted":"true"}))
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("include_deleted must be a boolean"));

    let error = records
        .execute(Operation::Distinct {
            field: "category".into(),
            filter: value!({}),
            options: value!("invalid options"),
        })
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("distinct options must be an object"));

    let error = records
        .execute(Operation::Aggregate {
            pipeline: value!([{"$group":{"records":{"$count":true}}}]),
            options: value!("invalid options"),
        })
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("aggregate options must be an object"));

    for sort in [
        value!({"records":0}),
        value!({"records":2}),
        value!({"records":1.5}),
        value!([["records", "desc"]]),
    ] {
        let error = records
            .execute(Operation::Aggregate {
                pipeline: value!([
                    {"$group":{"records":{"$count":true}}},
                    {"$sort":sort}
                ]),
                options: value!({}),
            })
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("aggregate: $sort direction must be 1 or -1"),
            "unexpected error: {error}"
        );
    }
    owner.close().await;
}

#[compio::test]
async fn dynamic_reads_reject_non_portable_value_semantics() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({
            "amount":{"type":"number", "precision":18, "scale":2},
            "document":{"type":"json"},
            "enabled":{"type":"boolean"},
            "secret":{"type":"string", "filterable":false}
        }),
    )
    .await;
    let records = owner.database.collection("records").unwrap();

    let error = records
        .find(value!({}), value!({"orderBy":{"amount":1}}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no portable sort order"));

    let error = records
        .execute(Operation::Distinct {
            field: "document".into(),
            filter: value!({}),
            options: value!({}),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no portable distinct equality"));

    let error = records
        .execute(Operation::Aggregate {
            pipeline: value!([{"$group":{"by":"amount","records":{"$count":true}}}]),
            options: value!({}),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no portable grouping equality"));

    let error = records
        .execute(Operation::Aggregate {
            pipeline: value!([
                {"$group":{"by":"enabled","records":{"$count":true}}},
                {"$having":{"enabled":{"$gt":false}}}
            ]),
            options: value!({}),
        })
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("predicate operator is not supported"));

    let error = records
        .execute(Operation::Aggregate {
            pipeline: value!([{"$group":{"by":"secret","records":{"$count":true}}}]),
            options: value!({}),
        })
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("field 'secret' is not filterable"));
    owner.close().await;
}
