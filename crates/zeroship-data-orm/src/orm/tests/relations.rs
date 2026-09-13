use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "title": {"type":"string", "required":true},
        "body": {"type":"string"},
        "parentId": {"type":"string", "refTarget":"nodes", "refColumn":"id", "relation":"parent"},
        "otherId": {"type":"string", "refTarget":"nodes", "refColumn":"id", "relation":"other"}
    })
}

async fn exercise(db: &Database) {
    let nodes = db.collection("nodes").unwrap();
    let Output::Rows { rows, .. } = nodes.insert(value!({"title":"parent"})).await.unwrap() else {
        panic!("expected insert")
    };
    let parent_id = rows[0]["id"].clone();
    nodes
        .insert(value!({"title":"child", "parentId":parent_id.clone()}))
        .await
        .unwrap();
    nodes.insert(value!({"title":"orphan"})).await.unwrap();
    let Output::Rows { rows, .. } = nodes
        .find(value!({"title":"child"}), value!({"with":{"parent":true}}))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert_eq!(rows[0]["parent"]["title"], value!("parent"));
    assert_eq!(rows[0]["parentId"], parent_id);
    let Output::Rows { rows, .. } = nodes
        .find(
            value!({"title":"child"}),
            value!({"select":["title"], "with":{"parent":true}}),
        )
        .await
        .unwrap()
    else {
        panic!("expected projected rows")
    };
    assert_eq!(rows[0]["parent"]["title"], value!("parent"));
    assert_eq!(rows[0].as_object().unwrap().len(), 2);
    let Output::Rows { rows, .. } = nodes
        .find(
            value!({"title":{"$in":["parent","orphan"]}}),
            value!({"with":{"parent":true}}),
        )
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["parent"].is_null()));
    nodes.delete(value!({"title":"parent"})).await.unwrap();
    let Output::Rows { rows, .. } = nodes
        .find(value!({"title":"child"}), value!({"with":{"parent":true}}))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert!(rows[0]["parent"].is_null());

    for spec in [
        value!({"parentId":true}),
        value!({"title":true}),
        value!({"parent":false}),
        value!({"parent":{}}),
    ] {
        assert!(
            nodes
                .find(value!({"title":"absent"}), value!({"with":spec}))
                .await
                .is_err()
        );
    }

    let (prepared, reads) = db.context.with(|| {
        let capture = crate::cdc::read_set::Active::begin(true);
        let prepared = nodes.find(value!({"title":"absent"}), value!({"with":{"parent":true}}));
        (prepared, capture.take())
    });
    assert!(
        reads
            .iter()
            .any(|read| read.collection == "nodes"
                && read.matches(&std::collections::HashMap::new()))
    );
    assert!(matches!(prepared.await.unwrap(), Output::Rows { rows, .. } if rows.is_empty()));
}

async fn batches(db: &Database) {
    let count = crate::sql::MAX_MEMBERSHIP_LIST_LEN + 1;
    assert!(count * 2 <= crate::sql::MAX_ROW_LIMIT as usize);
    let nodes = db.collection("nodes").unwrap();
    let documents = (0..count)
        .map(|index| value!({"title":format!("target-{index:08}")}))
        .collect::<Vec<_>>();
    let mut targets = Vec::new();
    for chunk in documents.chunks(crate::budgets::MAX_INSERT_MANY_BATCH) {
        let Output::Rows { rows, .. } = nodes
            .execute(Operation::InsertMany {
                documents: Value::Array(chunk.to_vec()),
            })
            .await
            .unwrap()
        else {
            panic!("expected rows")
        };
        targets.extend(rows);
    }
    let documents = targets.iter().enumerate().flat_map(|(index, target)| {
        [0, 1].map(|copy| value!({"title":format!("child-{index:08}-{copy}"), "parentId":target["id"].clone(), "otherId":target["id"].clone()}))
    }).collect::<Vec<_>>();
    for chunk in documents.chunks(crate::budgets::MAX_INSERT_MANY_BATCH) {
        nodes
            .execute(Operation::InsertMany {
                documents: Value::Array(chunk.to_vec()),
            })
            .await
            .unwrap();
    }
    let Output::Rows { rows, .. } = nodes.find(value!({"title":{"$like":"child-%"}}), value!({
        "limit":crate::sql::MAX_ROW_LIMIT, "orderBy":{"title":1}, "with":{"parent":true,"other":true}
    })).await.unwrap() else { panic!("expected rows") };
    assert_eq!(rows.len(), targets.len() * 2);
    for (index, pair) in rows.chunks(2).enumerate() {
        for row in pair {
            assert_eq!(row["parent"]["title"], value!(format!("target-{index:08}")));
            assert_eq!(row["parent"], row["other"]);
            assert_eq!(row["parentId"], row["parent"]["id"]);
        }
    }
}

#[compio::test]
async fn sqlite_relations_use_declared_references_and_target_visibility() {
    let fixture = CollectionFixture::sqlite("nodes", fields()).await;
    exercise(&fixture.database).await;
    batches(&fixture.database).await;
    fixture.close().await;
}

#[compio::test]
async fn postgres_relations_use_declared_references_and_target_visibility() {
    let fixture = CollectionFixture::postgres("nodes", fields()).await;
    exercise(&fixture.database).await;
    batches(&fixture.database).await;
    fixture.close().await;
}

#[compio::test]
async fn relation_fanout_obeys_the_read_result_budget() {
    let fixture = CollectionFixture::sqlite("nodes", fields()).await;
    let nodes = fixture.database.collection("nodes").unwrap();
    let count = crate::sql::MAX_ROW_LIMIT as usize;
    let body = "x".repeat(super::super::read::MAX_READ_RESULT_BYTES / count + 1);
    let Output::Rows { rows, .. } = nodes
        .insert(value!({"title":"target", "body":body}))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    let target = rows[0]["id"].clone();
    let documents = (0..count)
        .map(|_| value!({"title":"child", "parentId":target.clone()}))
        .collect::<Vec<_>>();
    for chunk in documents.chunks(crate::budgets::MAX_INSERT_MANY_BATCH) {
        nodes
            .execute(Operation::InsertMany {
                documents: Value::Array(chunk.to_vec()),
            })
            .await
            .unwrap();
    }
    let result = nodes
        .find(
            value!({"title":"child"}),
            value!({"limit":count, "with":{"parent":true}}),
        )
        .await;
    assert!(
        matches!(result, Err(DbError::ValidationFailed { message, .. }) if message.contains("size budget"))
    );
    fixture.close().await;
}
