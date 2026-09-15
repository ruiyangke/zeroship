use super::*;
use fixtures::CollectionFixture;

fn row(output: Output) -> Value {
    let Output::Rows { mut rows, .. } = output else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

async fn lifecycle(mut fixture: CollectionFixture) {
    let names = [
        ("created_at", "born"),
        ("updated_at", "touched"),
        ("created_by", "author"),
        ("updated_by", "editor"),
        ("version", "revision"),
        ("deleted_at", "removed"),
    ];
    fixture.rename_fields("entries", &names).await;
    let db = fixture
        .database
        .clone()
        .with_actor(Some("usr_author".into()));
    let entries = db.collection("entries").unwrap();
    let inserted = row(entries.insert(value!({"title":"first"})).await.unwrap());
    assert!(inserted["id"].as_str().unwrap().starts_with("entr_"));
    assert_eq!(inserted["revision"], value!(1));
    assert_eq!(inserted["author"], value!("usr_author"));
    assert_eq!(inserted["editor"], value!("usr_author"));
    assert!(inserted["born"].as_timestamp_micros().unwrap() > 0);
    assert_eq!(inserted["removed"], Value::Null);
    for (old, _) in names {
        assert!(inserted.get(old).is_none(), "{old}: {inserted}");
    }
    let key = inserted["id"].clone();
    let updated = row(entries
        .update(
            value!({"id":key.clone(), "revision":1}),
            value!({"title":"changed"}),
        )
        .await
        .unwrap());
    assert_eq!(updated["revision"], value!(2));
    assert_eq!(updated["born"], inserted["born"]);
    assert!(entries
        .update(
            value!({"id":key.clone(), "revision":1}),
            value!({"title":"stale"})
        )
        .await
        .is_err());
    assert!(entries
        .update(value!({"id":key.clone()}), value!({"born":0}))
        .await
        .is_err());
    assert!(entries
        .update(value!({"id":key.clone()}), value!({"removed":0}))
        .await
        .is_err());

    let anonymous = fixture.database.collection("entries").unwrap();
    let updated = row(anonymous
        .update(value!({"id":key.clone()}), value!({"title":"changed"}))
        .await
        .unwrap());
    assert_eq!(updated["revision"], value!(3));
    assert_eq!(updated["editor"], Value::Null);
    assert_eq!(updated["author"], value!("usr_author"));
    let upserted = row(entries
        .execute(Operation::Upsert {
            document: value!({"title":"changed"}),
            conflict_fields: value!(["title"]),
        })
        .await
        .unwrap());
    assert_eq!(upserted["id"], key);
    assert_eq!(upserted["born"], inserted["born"]);
    assert_eq!(upserted["revision"], value!(4));

    let deleted = row(entries.delete(value!({"id":key.clone()})).await.unwrap());
    assert!(deleted["removed"].as_timestamp_micros().unwrap() > 0);
    assert_eq!(deleted["revision"], value!(5));
    let Output::Rows { rows, .. } = entries.find(value!({}), value!({})).await.unwrap() else {
        panic!("rows")
    };
    assert!(rows.is_empty());
    let Output::Rows { rows, .. } = entries.delete(value!({"id":key.clone()})).await.unwrap()
    else {
        panic!("rows")
    };
    assert!(rows.is_empty());
    let restored = row(entries
        .execute(Operation::Restore {
            filter: value!({"id":key.clone()}),
            many: false,
        })
        .await
        .unwrap());
    assert_eq!(restored["removed"], Value::Null);
    assert_eq!(restored["revision"], value!(6));
    assert_eq!(
        row(entries.find(value!({}), value!({})).await.unwrap())["id"],
        key
    );
    let Output::Rows { rows, .. } = entries
        .execute(Operation::Restore {
            filter: value!({"id":key.clone()}),
            many: false,
        })
        .await
        .unwrap()
    else {
        panic!("rows")
    };
    assert!(rows.is_empty());
    entries
        .execute(Operation::Purge {
            filter: value!({"id":key}),
            many: false,
        })
        .await
        .unwrap();
    assert!(matches!(
        entries.count(value!({}), value!({})).await.unwrap(),
        Output::Count(0)
    ));
    fixture.close().await;
}

#[compio::test]
async fn sqlite_generators_follow_renamed_columns() {
    lifecycle(
        CollectionFixture::sqlite(
            "entries",
            value!({"title":{"type":"string", "unique":true}}),
        )
        .await,
    )
    .await;
}

#[compio::test]
async fn postgres_generators_follow_renamed_columns() {
    lifecycle(
        CollectionFixture::postgres(
            "entries",
            value!({"title":{"type":"string", "unique":true}}),
        )
        .await,
    )
    .await;
}

async fn familiar_names_are_ordinary(fixture: CollectionFixture) {
    let original = &fixture.database;
    let mut schema = original.context.with(|| {
        crate::descriptor::collection_schema(&original.binding, "entries")
            .unwrap()
            .as_ref()
            .clone()
    });
    for field in schema.values_mut() {
        field.assignment = None;
        field.soft_delete = false;
        field.concurrency = false;
        field.writable = true;
    }
    let db = Database::from_schema(
        original.binding.clone(),
        original.backend.clone(),
        Schema::new([("entries".into(), CollectionSchema::new(schema))]),
    )
    .unwrap()
    .with_actor(Some("request_actor".into()));
    let entries = db.collection("entries").unwrap();
    let inserted = row(entries
        .insert(value!({
            "id":"chosen", "title":"ordinary", "created_at":1700000000000_i64,
            "updated_at":1700000000000_i64, "created_by":"supplied", "updated_by":"supplied",
            "version":99, "deleted_at":1700000000000_i64
        }))
        .await
        .unwrap());
    assert_eq!(inserted["id"], value!("chosen"));
    assert_eq!(inserted["created_by"], value!("supplied"));
    assert_eq!(
        row(entries.find(value!({}), value!({})).await.unwrap())["version"],
        value!(99)
    );
    let updated = row(entries
        .update(
            value!({"id":"chosen"}),
            value!({"created_by":"changed", "version":10, "deleted_at":Value::Null}),
        )
        .await
        .unwrap());
    assert_eq!(updated["created_by"], value!("changed"));
    assert_eq!(updated["updated_by"], value!("supplied"));
    assert_eq!(updated["version"], value!(10));
    assert_eq!(updated["updated_at"], inserted["updated_at"]);
    entries.delete(value!({"id":"chosen"})).await.unwrap();
    assert!(matches!(
        entries
            .count(value!({}), value!({"includeDeleted":true}))
            .await
            .unwrap(),
        Output::Count(0)
    ));
    fixture.close().await;
}

#[compio::test]
async fn sqlite_familiar_names_are_ordinary_without_assignments() {
    familiar_names_are_ordinary(
        CollectionFixture::sqlite("entries", value!({"title":{"type":"string"}})).await,
    )
    .await;
}

#[compio::test]
async fn postgres_familiar_names_are_ordinary_without_assignments() {
    familiar_names_are_ordinary(
        CollectionFixture::postgres("entries", value!({"title":{"type":"string"}})).await,
    )
    .await;
}
