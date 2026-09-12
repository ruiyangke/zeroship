use super::fixtures::CollectionFixture;
use super::*;
use crate::sql::{CompareOp, JoinKind, Operand, Predicate};

const COLLECTION: &str = "__zeroship_workflow_app_state";

fn fields() -> Value {
    value!({
        "id":{"type":"string", "required":true, "primaryKey":true},
        "peer_id":{"type":"string"},
        "label":{"type":"string", "required":true}
    })
}

fn rows(output: Output) -> Vec<Value> {
    match output {
        Output::Rows { rows, .. } => rows,
        output => panic!("expected rows, got {output:?}"),
    }
}

async fn exercise(database: &Database) {
    let collection = database.collection(COLLECTION).unwrap();
    collection
        .insert(value!({"id":"child", "peer_id":null, "label":"child"}))
        .await
        .unwrap();

    database
        .transaction(|transaction| async move {
            transaction
                .collection(COLLECTION)?
                .insert(value!({"id":"parent", "peer_id":"child", "label":"parent"}))
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    let found = rows(
        collection
            .find(value!({"id":"parent"}), value!({}))
            .await
            .unwrap(),
    );
    assert_eq!(found[0]["label"], value!("parent"));

    let parent = ReadSource::new(COLLECTION, "parent");
    let child = ReadSource::new(COLLECTION, "child");
    let mut query = ReadQuery::new(parent.clone());
    query.joins.push(ReadJoin {
        kind: JoinKind::Inner,
        source: child.clone(),
        on: Predicate::compare(
            Operand::Path(parent.column("peer_id").unwrap()),
            CompareOp::Eq,
            Operand::Path(child.column("id").unwrap()),
        ),
    });
    query.filter = Predicate::compare(
        Operand::Path(parent.column("id").unwrap()),
        CompareOp::Eq,
        Operand::Lit(crate::sql::Literal::Text("parent".into())),
    );
    query.projection = vec![
        ReadProjection::Row {
            output: "parent".into(),
            source: parent.alias,
            fields: Some(vec!["label".into()]),
            optional: false,
        },
        ReadProjection::Row {
            output: "child".into(),
            source: child.alias,
            fields: Some(vec!["label".into()]),
            optional: false,
        },
    ];
    let joined = rows(database.read(query).await.unwrap());
    assert_eq!(joined[0]["parent"]["label"], value!("parent"));
    assert_eq!(joined[0]["child"]["label"], value!("child"));
}

#[compio::test]
async fn sqlite_exposes_every_table_in_the_bound_creator_schema() {
    let fixture = CollectionFixture::sqlite_from_table_definition(
        COLLECTION,
        fields(),
        "id TEXT PRIMARY KEY, peer_id TEXT, label TEXT NOT NULL",
    )
    .await;
    exercise(&fixture.database).await;
    fixture.close().await;
}

#[compio::test]
async fn postgres_exposes_every_table_in_the_bound_creator_schema() {
    let fixture = CollectionFixture::postgres_from_table_definition(
        COLLECTION,
        fields(),
        "id TEXT PRIMARY KEY, peer_id TEXT, label TEXT NOT NULL",
    )
    .await;
    exercise(&fixture.database).await;
    fixture.close().await;
}
