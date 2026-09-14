use super::fixtures::CollectionFixture;
use super::*;

include!("../../../tests/fixtures/updates_schema.rs");
updates_schema!(pub update_schema);
use update_schema::documents;

#[derive(Debug, FromRow)]
#[orm(entity = documents)]
struct Document {
    label: String,
    payload: Value,
}

#[derive(Insertable)]
#[orm(entity = documents)]
struct NewDocument {
    label: String,
    payload: Value,
}

#[derive(Default, Changeset)]
#[orm(entity = documents)]
struct EditDocument {
    label: Change<String>,
    payload: Change<Value>,
}

#[compio::test]
async fn sqlite_typed_sets_keep_operator_shaped_json_literal() {
    let owner = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    exercise_literal_sets(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_typed_sets_keep_operator_shaped_json_literal() {
    let owner = CollectionFixture::postgres_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    exercise_literal_sets(&owner.database).await;
    owner.close().await;
}

async fn exercise_literal_sets(db: &Database) {
    let documents = db.entity::<documents::Entity>().unwrap();
    let row: Document = documents
        .insert(NewDocument {
            label: "original".into(),
            payload: value!({"initial":true}),
        })
        .await
        .unwrap();
    for payload in [
        value!({"$inc":2}),
        value!({"$set":{"nested":true}}),
        value!({"$mul":2,"description":"literal JSON"}),
    ] {
        let updated: Document = documents
            .update(
                documents::label.eq(row.label.clone()).unwrap(),
                EditDocument {
                    payload: Change::Set(payload.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.payload, payload);
        assert_eq!(updated.label, "original");
        let patch = documents::payload
            .set(payload.clone())
            .unwrap()
            .and(documents::label.set("original".to_owned()).unwrap())
            .unwrap();
        let updated: Document = documents
            .update(documents::label.eq(row.label.clone()).unwrap(), patch)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.payload, payload);
    }
}

#[compio::test]
async fn sqlite_typed_queries_keep_operator_shaped_json_literal() {
    let owner = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    exercise_literal_filters(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_typed_queries_keep_operator_shaped_json_literal() {
    let owner = CollectionFixture::postgres_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    exercise_literal_filters(&owner.database).await;
    owner.close().await;
}

async fn exercise_literal_filters(db: &Database) {
    let documents = db.entity::<documents::Entity>().unwrap();
    for payload in [
        value!({"$eq":2}),
        value!({"$and":[1,2]}),
        value!({"ordinary":2}),
    ] {
        let _: Document = documents
            .insert(NewDocument {
                label: "literal".into(),
                payload: payload.clone(),
            })
            .await
            .unwrap();
        let rows: Vec<Document> = documents
            .find(
                documents::payload.eq(payload.clone()).unwrap(),
                FindOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payload, payload);
    }
}

async fn scalar_json_filters(db: &Database) {
    let table = db.entity::<documents::Entity>().unwrap();
    let expected = [
        value!(u64::MAX),
        value!(u64::MAX.to_string()),
        value!(true),
        value!("true"),
        value!({"$eq":2}),
    ];
    for payload in &expected {
        table
            .insert::<_, Document>(NewDocument {
                label: "scalar".into(),
                payload: payload.clone(),
            })
            .await
            .unwrap();
    }
    let d = table.alias("d").unwrap();
    for payload in expected {
        for predicate in [
            d.column(documents::payload).eq(payload.clone()).unwrap(),
            d.column(documents::payload)
                .select::<Value>()
                .eq(payload.clone())
                .unwrap(),
            d.column(documents::payload)
                .select_optional::<Value>()
                .eq(payload.clone())
                .unwrap(),
        ] {
            let rows = db
                .from(&d)
                .filter(predicate)
                .select(d.column(documents::payload).select::<Value>())
                .unwrap()
                .all()
                .await
                .unwrap();
            assert_eq!(rows, vec![payload.clone()]);
        }
    }
}

#[compio::test]
async fn sqlite_scalar_json_comparisons_preserve_value_types() {
    let owner = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    scalar_json_filters(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_scalar_json_comparisons_preserve_value_types() {
    let owner = CollectionFixture::postgres_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    scalar_json_filters(&owner.database).await;
    owner.close().await;
}

#[test]
fn composing_patches_refuses_duplicate_columns() {
    for value in ["first", "second"] {
        let first = documents::label.set("first".to_owned()).unwrap();
        let second = documents::label.set(value.to_owned()).unwrap();
        let error = first.and(second).unwrap_err();
        assert!(matches!(
            error,
            DbError::ValidationFailed {
                code: "invalid_update",
                ..
            }
        ));
    }
}
