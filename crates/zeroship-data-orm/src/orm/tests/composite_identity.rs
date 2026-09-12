use super::fixtures::CollectionFixture;
use super::*;

schema!(pub composite = "../../../tests/fixtures/composite.runtime.json");
use composite::records;

#[derive(Debug, FromRow)]
#[orm(entity = records)]
struct Summary {
    label: String,
    secret: Option<String>,
}

#[derive(Insertable)]
#[orm(entity = records)]
struct NewRecord<'a> {
    app_key: &'a str,
    run_key: &'a str,
    generation: i64,
    label: &'a str,
    secret: Option<&'a str>,
}

#[derive(Changeset)]
#[orm(entity = records)]
struct EditId {
    id: Change<Option<String>>,
}

fn rows(output: Output) -> Vec<Value> {
    let Output::Rows { rows, .. } = output else {
        panic!("expected rows")
    };
    rows
}

async fn fixture(postgres: bool) -> CollectionFixture {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("composite_identity", &"5".repeat(64))
        .unwrap();
    let source = ProjectKeySource::supplied(keys.clone());
    let mut owner = if postgres {
        CollectionFixture::postgres_with_keys(
            "records",
            value!({"label":{"type":"string"}}),
            source,
        )
        .await
    } else {
        CollectionFixture::sqlite_with_keys("records", value!({"label":{"type":"string"}}), source)
            .await
    };
    keys.bind_app(owner.database.binding.app_id(), "composite_identity")
        .unwrap();
    owner
        .replace_from_migration_with_policy(
            "records",
            include_str!("../../../tests/fixtures/composite-migration.json"),
            zeroship_migrate_server::policy::PLATFORM_CEILING_TOML,
        )
        .await;
    owner
        .database
        .install_mask_policy(value!({"support":["pii"]}))
        .unwrap();
    owner
}

#[test]
fn models_are_generated_by_the_migration_engine() {
    let migration: zeroship_migrate::model::ir::MigrationIr = serde_json::from_str(include_str!(
        "../../../tests/fixtures/composite-migration.json"
    ))
    .unwrap();
    let policy = zeroship_migrate::effective_policy_from_charter_toml(
        zeroship_migrate_server::policy::PLATFORM_CEILING_TOML,
    )
    .unwrap();
    let generated = zeroship_migrate::render_artifacts(
        zeroship_migrate::shipping_vendors(),
        &migration.ops,
        &zeroship_migrate_postgres::DIALECT,
        "orm_fixture",
        &policy,
    )
    .unwrap();
    assert_eq!(
        generated.runtime_json,
        include_str!("../../../tests/fixtures/composite.runtime.json")
    );
}

async fn exercise(postgres: bool) {
    let owner = fixture(postgres).await;
    let db = &owner.database;
    let collection = db.collection("records").unwrap();
    let entity = db.entity::<records::Entity>().unwrap();
    for (app_key, run_key, generation, label) in [
        ("a", "shared", 1, "first"),
        ("b", "shared", 1, "other_app"),
        ("a", "other", 1, "other_run"),
        ("a", "shared", 2, "other_generation"),
    ] {
        entity
            .insert::<_, Summary>(NewRecord {
                app_key,
                run_key,
                generation,
                label,
                secret: Some(label),
            })
            .await
            .unwrap();
    }
    entity
        .update::<_, Summary>(
            records::label.eq("first").unwrap(),
            EditId {
                id: Change::Set(Some("ordinary".into())),
            },
        )
        .await
        .unwrap();
    let key = value!({"app_key":"a", "run_key":"shared", "generation":1});
    collection
        .update(
            key.clone(),
            value!({"secret":"changed", "masked":"12345678", "parent":"other_app"}),
        )
        .await
        .unwrap();
    for (field, value) in [
        ("app_key", value!("c")),
        ("run_key", value!("different")),
        ("generation", value!(3)),
    ] {
        for patch in [
            Value::Object([(field.to_owned(), value.clone())].into()),
            value!({"$set": Value::Object([(field.to_owned(), value)].into())}),
        ] {
            let error = collection.update(key.clone(), patch).await.unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::ValidationFailed {
                        code: "immutable_primary_key",
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }
    let stored = rows(collection.find(value!({}), value!({})).await.unwrap());
    for row in stored {
        assert_eq!(
            row["secret"],
            if row["label"] == "first" {
                value!("changed")
            } else {
                row["label"].clone()
            }
        );
    }
    for column in ["secret", "masked"] {
        let projected = rows(
            collection
                .find(key.clone(), value!({"select":[column]}))
                .await
                .unwrap(),
        )
        .remove(0);
        assert_eq!(projected.as_object().unwrap().len(), 1);
        if column == "masked" {
            let token = projected[column]["_meta"]["row_pk"].as_str().unwrap();
            let schema = db
                .context
                .with(|| crate::descriptor::collection_schema(&db.binding, "records").unwrap());
            assert_eq!(
                crate::row_identity::from_token(&schema, token).unwrap(),
                *key.as_object().unwrap()
            );
        } else {
            assert_eq!(projected[column], value!("changed"));
        }
    }
    let unmasked = rows(collection.find(key.clone(), value!({"select":["masked"], "unmask":["masked"], "actor":{"kind":"support", "id":"usr_reader"}, "unmaskReason":"composite key regression"})).await.unwrap());
    assert_eq!(unmasked, vec![value!({"masked":"12345678"})]);

    let parent = entity.alias("p").unwrap();
    let child = entity.alias("c").unwrap();
    let joined = db
        .from(&parent)
        .left_join(
            &child,
            parent
                .column(records::parent)
                .eq_column(child.column(records::label))
                .unwrap(),
        )
        .unwrap()
        .filter(parent.column(records::label).eq("first").unwrap())
        .select((parent.row::<Summary>(), child.optional_row::<Summary>()))
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(joined[0].0.secret.as_deref(), Some("changed"));
    assert_eq!(joined[0].1.as_ref().unwrap().label, "other_app");
    let absent = db
        .from(&parent)
        .left_join(
            &child,
            parent
                .column(records::parent)
                .eq_column(child.column(records::label))
                .unwrap(),
        )
        .unwrap()
        .filter(
            parent
                .column(records::label)
                .eq("other_generation")
                .unwrap(),
        )
        .select(child.optional_row::<Summary>())
        .unwrap()
        .all()
        .await
        .unwrap();
    assert!(absent[0].is_none());

    let upserted = rows(collection.execute(Operation::Upsert {
        document:value!({"app_key":"replacement", "run_key":"new", "generation":9, "label":"first", "secret":"upserted"}),
        conflict_fields:value!(["label"]),
    }).await.unwrap()).remove(0);
    for field in ["app_key", "run_key", "generation"] {
        assert_eq!(upserted[field], key[field]);
    }
    assert_eq!(upserted["secret"], value!("upserted"));
    assert!(matches!(
        collection
            .execute(Operation::Update {
                filter: value!({"run_key":"shared"}),
                patch: value!({"secret":"batch"}),
                many: true
            })
            .await
            .unwrap(),
        Output::Count(3)
    ));
    let updated = rows(
        collection
            .find(value!({"run_key":"shared"}), value!({"select":["secret"]}))
            .await
            .unwrap(),
    );
    assert!(updated.iter().all(|row| row["secret"] == "batch"));
    let rollback: Result<(), DbError> = db
        .transaction(|tx| async move {
            tx.collection("records")?
                .update(
                    value!({"app_key":"a", "run_key":"shared", "generation":1}),
                    value!({"secret":"discard"}),
                )
                .await?;
            Err(DbError::internal("rollback composite write"))
        })
        .await;
    assert!(rollback.is_err());
    assert_eq!(
        rows(collection.find(key.clone(), value!({})).await.unwrap())[0]["secret"],
        value!("batch")
    );
    let removed = rows(
        collection
            .execute(Operation::Purge {
                filter: key.clone(),
                many: false,
            })
            .await
            .unwrap(),
    );
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0]["label"], value!("first"));
    assert_eq!(
        rows(collection.find(value!({}), value!({})).await.unwrap()).len(),
        3
    );
    owner.close().await;
}

#[compio::test]
async fn sqlite_compound_keys_preserve_row_boundaries() {
    exercise(false).await;
}
#[compio::test]
async fn postgres_compound_keys_preserve_row_boundaries() {
    exercise(true).await;
}

#[compio::test]
async fn postgres_concurrent_upsert_keeps_the_winning_generation() {
    let owner = fixture(true).await;
    let db = &owner.database;
    let fields = db
        .context
        .with(|| crate::descriptor::collection_schema(&db.binding, "records").unwrap());
    let other = Database::from_schema(
        db.binding.clone(),
        db.backend.clone(),
        vec![("records".into(), fields.as_ref().clone())],
    )
    .unwrap();
    let (inserted_tx, inserted_rx) = futures::channel::oneshot::channel();
    let (commit_tx, commit_rx) = futures::channel::oneshot::channel();
    let first = db.transaction(|tx| async move {
        tx.collection("records")?.insert(value!({"app_key":"a", "run_key":"shared", "generation":1, "label":"conflict", "secret":"first"})).await?;
        inserted_tx.send(()).unwrap();
        commit_rx.await.unwrap();
        Ok(())
    });
    let second = async {
        inserted_rx.await.unwrap();
        other.collection("records").unwrap().execute(Operation::Upsert {
            document:value!({"app_key":"a", "run_key":"shared", "generation":2, "label":"conflict", "secret":"second"}),
            conflict_fields:value!(["label"]),
        }).await
    };
    let release = async {
        owner.wait_for_upsert_conflict().await;
        commit_tx.send(()).unwrap();
    };
    let (first, second, ()) = futures::join!(first, second, release);
    first.unwrap();
    assert_eq!(rows(second.unwrap())[0]["generation"], value!(1));
    let stored = rows(
        other
            .collection("records")
            .unwrap()
            .find(value!({}), value!({}))
            .await
            .unwrap(),
    );
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0]["secret"], value!("second"));
    owner.close().await;
}

async fn timestamp_key(postgres: bool) {
    let mut owner = fixture(postgres).await;
    let mut migration: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/composite-migration.json"
    ))
    .unwrap();
    migration["ops"][0]["columns"][2]["type"] = serde_json::json!("timestamp");
    owner
        .replace_from_migration_with_policy(
            "records",
            &migration.to_string(),
            zeroship_migrate_server::policy::PLATFORM_CEILING_TOML,
        )
        .await;
    owner
        .database
        .install_mask_policy(value!({"support":["pii"]}))
        .unwrap();
    let collection = owner.database.collection("records").unwrap();
    for instant in [-1, 1] {
        collection.insert(value!({"app_key":"a", "run_key":"shared", "generation":Value::Timestamp(instant), "label":format!("instant_{instant}"), "masked":format!("private_{instant}")})).await.unwrap();
    }
    let key = value!({"app_key":"a", "run_key":"shared", "generation":Value::Timestamp(-1)});
    let unmasked = rows(collection.find(key, value!({"select":["masked"], "unmask":["masked"], "actor":{"kind":"support", "id":"usr_reader"}, "unmaskReason":"timestamp identity regression"})).await.unwrap());
    assert_eq!(unmasked, vec![value!({"masked":"private_-1"})]);
    owner.close().await;
}

#[compio::test]
async fn sqlite_unmask_binds_timestamp_key_storage() {
    timestamp_key(false).await;
}

#[compio::test]
async fn postgres_unmask_binds_timestamp_key_storage() {
    timestamp_key(true).await;
}
