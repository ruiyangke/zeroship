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
