use super::fixtures::CollectionFixture;
use super::*;

include!("../../../tests/fixtures/updates_schema.rs");
updates_schema!(pub mutation_schema);
use mutation_schema::documents;

#[derive(Debug, FromRow)]
#[orm(entity = documents)]
struct Document {
    id: String,
    label: String,
    payload: Value,
    version: i64,
    created_by: Option<String>,
    updated_by: Option<String>,
    deleted_at: Option<UtcInstant>,
}

#[derive(Insertable)]
#[orm(entity = documents)]
struct NewDocument {
    label: String,
    payload: Value,
}

fn document(label: &str, payload: Value) -> NewDocument {
    NewDocument {
        label: label.into(),
        payload,
    }
}

#[compio::test]
async fn sqlite_fixture_databases_have_independent_broker_identities() {
    let first = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    let second = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    assert_ne!(first.sqlite_file, second.sqlite_file);
    assert_ne!(
        first.database.binding().app_id(),
        second.database.binding().app_id()
    );
    assert!(zeroship_core::app_id::AppId::parse(first.database.binding().app_id()).is_ok());
    assert!(zeroship_core::app_id::AppId::parse(second.database.binding().app_id()).is_ok());
    first.close().await;
    second.close().await;
    let schema = value!({"id":{"type":"string", "required":true, "primaryKey":true}});
    let first = CollectionFixture::sqlite_from_table_definition(
        "records",
        schema.clone(),
        "id TEXT PRIMARY KEY",
    )
    .await;
    let second =
        CollectionFixture::sqlite_from_table_definition("records", schema, "id TEXT PRIMARY KEY")
            .await;
    assert_ne!(
        first.database.binding().app_id(),
        second.database.binding().app_id()
    );
    assert!(zeroship_core::app_id::AppId::parse(first.database.binding().app_id()).is_ok());
    assert!(zeroship_core::app_id::AppId::parse(second.database.binding().app_id()).is_ok());
    first.close().await;
    second.close().await;
}

#[compio::test]
async fn typed_insert_many_stops_consuming_at_the_shared_batch_budget() {
    let fixture = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    let documents = fixture.database.entity::<documents::Entity>().unwrap();
    let limit = crate::budgets::MAX_INSERT_MANY_BATCH;
    let consumed = Cell::new(0);
    let input = std::iter::from_fn(|| {
        consumed.set(consumed.get() + 1);
        assert!(
            consumed.get() <= limit + 1,
            "the ORM kept consuming an oversized iterator"
        );
        Some(document("bounded", value!({})))
    });
    let error = documents
        .insert_many::<_, Document>(input)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
    assert_eq!(consumed.get(), limit + 1);
    assert!(documents
        .find::<Document>(Filter::all(), Default::default())
        .await
        .unwrap()
        .is_empty());
    let accepted: Vec<Document> = documents
        .insert_many(std::iter::repeat_with(|| document("within budget", value!({}))).take(limit))
        .await
        .unwrap();
    assert_eq!(accepted.len(), limit);
    fixture.close().await;
}

#[test]
fn delete_notification_intent_follows_the_schema_lifecycle() {
    for registration in [
        crate::sql::registration::SqlRegistration::sqlite(),
        crate::sql::registration::SqlRegistration::postgres(),
    ] {
        for soft_delete in [false, true] {
            let context = crate::OrmContext::new();
            context.with(|| {
                let binding = DbBinding::cold_start("typed_delete_intent");
                let mut schema = documents::Entity::schema().fields().clone();
                if !soft_delete {
                    schema.shift_remove("deleted_at");
                }
                crate::descriptor::install_collections(
                    &binding,
                    Schema::new([("documents".into(), CollectionSchema::new(schema))]),
                )
                .unwrap();
                for many in [false, true] {
                    for typed in [false, true] {
                        let route =
                            CapturedRoute::pool_for_tests(binding.app_id(), registration.clone());
                        let prepared = if typed {
                            PreparedOperation::new_model_mutation(
                                binding.clone(),
                                "documents",
                                route,
                                None,
                                Filter::<documents::Entity>::all().into_predicate(),
                                mutations::Mutation::Delete,
                                many,
                            )
                        } else {
                            PreparedOperation::new(
                                binding.clone(),
                                "documents",
                                route,
                                None,
                                Operation::Delete {
                                    filter: value!({}),
                                    many,
                                },
                            )
                        }
                        .unwrap();
                        let Plan::Mutation { operation, .. } = prepared.plan else {
                            panic!("delete must prepare a mutation");
                        };
                        assert_eq!(
                            operation,
                            if soft_delete {
                                ChangeOp::Update
                            } else {
                                ChangeOp::Delete
                            },
                            "the notification must describe the database mutation"
                        );
                    }
                }
            });
        }
    }
}

#[compio::test]
async fn sqlite_typed_bulk_and_lifecycle_mutations_share_orm_semantics() {
    let fixture = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    exercise_mutations(&fixture.database).await;
    fixture.close().await;
}

#[compio::test]
async fn postgres_typed_bulk_and_lifecycle_mutations_share_orm_semantics() {
    let fixture = CollectionFixture::postgres_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    exercise_mutations(&fixture.database).await;
    fixture.close().await;
}

async fn exercise_mutations(db: &Database) {
    let db = db.clone().with_actor(Some("writer".into()));
    let documents = db.entity::<documents::Entity>().unwrap();
    let literal = value!({"$eq": "literal"});
    let inserted: Vec<Document> = documents
        .insert_many([
            document("first", literal.clone()),
            document("second", literal.clone()),
            document("untouched", value!({"other":true})),
        ])
        .await
        .unwrap();
    assert_eq!(inserted.len(), 3);
    assert_ne!(inserted[0].id, inserted[1].id);
    assert!(inserted
        .iter()
        .all(|row| row.created_by.as_deref() == Some("writer")));
    assert_eq!(
        documents
            .update_many(
                documents::payload.eq(literal.clone()).unwrap(),
                documents::payload.set(value!({"$inc":2})).unwrap(),
            )
            .await
            .unwrap(),
        2
    );
    let changed = documents
        .find::<Document>(
            documents::payload.eq(value!({"$inc":2})).unwrap(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(changed.len(), 2);
    assert!(changed.iter().all(|row| row.version == 2));
    assert!(changed
        .iter()
        .all(|row| row.updated_by.as_deref() == Some("writer")));
    assert_eq!(
        documents
            .delete_many(documents::payload.eq(value!({"$inc":2})).unwrap())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        documents
            .delete_many(documents::payload.eq(value!({"$inc":2})).unwrap())
            .await
            .unwrap(),
        0
    );
    let deleted = documents
        .find::<Document>(
            documents::payload.eq(value!({"$inc":2})).unwrap(),
            FindOptions {
                include_deleted: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(deleted
        .iter()
        .all(|row| row.deleted_at.is_some() && row.version == 3));
    let restored: Document = documents
        .restore(documents::id.eq(inserted[0].id.clone()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(restored.deleted_at.is_none());
    assert_eq!(restored.version, 4);
    assert!(documents
        .restore::<Document>(documents::id.eq(restored.id.clone()).unwrap())
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        documents
            .restore_many(documents::payload.eq(value!({"$inc":2})).unwrap())
            .await
            .unwrap(),
        1
    );
    assert_eq!(documents.restore_many(Filter::all()).await.unwrap(), 0);
    let purged: Document = documents
        .purge(documents::id.eq(restored.id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(purged.label, "first");
    assert!(documents
        .purge::<Document>(documents::id.eq(purged.id).unwrap())
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        documents
            .purge_many(documents::payload.eq(value!({"$inc":2})).unwrap())
            .await
            .unwrap(),
        1
    );
    let remaining = documents
        .find::<Document>(Filter::all(), Default::default())
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].label, "untouched");
    assert_eq!(remaining[0].version, 1);

    let rollback: Result<(), DbError> = db
        .transaction(|tx| async move {
            let documents = tx.entity::<documents::Entity>()?;
            let _: Vec<Document> = documents
                .insert_many([document("rollback", value!({}))])
                .await?;
            documents
                .update_many(
                    Filter::all(),
                    documents::payload.set(value!({"rollback":true}))?,
                )
                .await?;
            documents.delete_many(Filter::all()).await?;
            documents.restore_many(Filter::all()).await?;
            documents.purge_many(Filter::all()).await?;
            Err(DbError::internal("roll back typed mutations"))
        })
        .await;
    assert_eq!(
        rollback.unwrap_err().message_str(),
        "roll back typed mutations"
    );
    let remaining = documents
        .find::<Document>(Filter::all(), Default::default())
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].version, 1);

    let escaped = db
        .transaction(|tx| async move { tx.entity::<documents::Entity>() })
        .await
        .unwrap();
    for error in [
        escaped
            .insert_many::<_, Document>([document("expired", value!({}))])
            .await
            .unwrap_err(),
        escaped
            .update_many(Filter::all(), documents::label.set("expired").unwrap())
            .await
            .unwrap_err(),
        escaped.delete_many(Filter::all()).await.unwrap_err(),
        escaped
            .restore::<Document>(Filter::all())
            .await
            .unwrap_err(),
        escaped.restore_many(Filter::all()).await.unwrap_err(),
        escaped.purge::<Document>(Filter::all()).await.unwrap_err(),
        escaped.purge_many(Filter::all()).await.unwrap_err(),
        escaped
            .upsert::<_, Document>(
                document("expired", value!({})),
                ConflictTarget::new(documents::label),
            )
            .await
            .unwrap_err(),
    ] {
        assert!(
            matches!(
                error,
                DbError::ValidationFailed {
                    code: "transaction_scope_expired",
                    ..
                }
            ),
            "{error}"
        );
    }
    let prepared = db
        .transaction(|tx| async move {
            Ok(tx
                .entity::<documents::Entity>()?
                .update_many(Filter::all(), documents::label.set("expired")?))
        })
        .await
        .unwrap();
    assert!(matches!(
        prepared.await.unwrap_err(),
        DbError::ValidationFailed {
            code: "transaction_scope_expired",
            ..
        }
    ));
}

#[compio::test]
async fn sqlite_typed_upserts_use_live_unique_targets_and_atomic_inserts() {
    let fixture = CollectionFixture::sqlite_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    fixture.add_unique_index("documents", &["label"]).await;
    fixture
        .add_unique_index("documents", &["label", "payload"])
        .await;
    exercise_upserts(&fixture.database).await;
    fixture.close().await;
}

#[compio::test]
async fn postgres_typed_upserts_use_live_unique_targets_and_atomic_inserts() {
    let fixture = CollectionFixture::postgres_native(
        "documents",
        documents::Entity::schema().clone(),
        super::fixtures::document_migration_fields(),
    )
    .await;
    fixture.add_unique_index("documents", &["label"]).await;
    fixture
        .add_unique_index("documents", &["label", "payload"])
        .await;
    exercise_upserts(&fixture.database).await;
    fixture.close().await;
}

async fn exercise_upserts(db: &Database) {
    let documents = db.entity::<documents::Entity>().unwrap();
    assert!(documents
        .insert_many::<_, Document>([
            document("duplicate", value!({"first":true})),
            document("duplicate", value!({"second":true})),
        ])
        .await
        .is_err());
    assert!(documents
        .find::<Document>(Filter::all(), Default::default())
        .await
        .unwrap()
        .is_empty());
    let inserted: Document = documents
        .upsert(
            document("upsert", value!({"initial":true})),
            ConflictTarget::new(documents::label),
        )
        .await
        .unwrap();
    let updated: Document = documents
        .upsert(
            document("upsert", value!({"$set": "literal"})),
            ConflictTarget::new(documents::label),
        )
        .await
        .unwrap();
    assert_eq!(updated.id, inserted.id);
    assert_eq!(updated.payload, value!({"$set":"literal"}));
    assert_eq!(updated.version, inserted.version + 1);
    assert!(documents
        .upsert::<_, Document>(
            document("invalid", value!({})),
            ConflictTarget::new(documents::payload),
        )
        .await
        .is_err());
    let error = documents
        .upsert::<_, Document>(
            document("invalid", value!({})),
            ConflictTarget::new(documents::label).and(documents::label),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
    let rollback: Result<(), DbError> = db
        .transaction(|tx| async move {
            let documents = tx.entity::<documents::Entity>()?;
            let _: Document = documents
                .upsert(
                    document("upsert", value!({"rollback":true})),
                    ConflictTarget::new(documents::label),
                )
                .await?;
            let _: Document = documents
                .upsert(
                    document("rolled-back-insert", value!({})),
                    ConflictTarget::new(documents::label),
                )
                .await?;
            Err(DbError::internal("roll back upserts"))
        })
        .await;
    assert_eq!(rollback.unwrap_err().message_str(), "roll back upserts");
    let remaining = documents
        .find::<Document>(Filter::all(), Default::default())
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, inserted.id);
    assert_eq!(remaining[0].payload, updated.payload);
    assert_eq!(remaining[0].version, updated.version);
    let inserted: Document = documents
        .upsert(
            document("composite", value!({"$eq":"literal"})),
            ConflictTarget::new(documents::label).and(documents::payload),
        )
        .await
        .unwrap();
    let updated: Document = documents
        .upsert(
            document("composite", value!({"$eq":"literal"})),
            ConflictTarget::new(documents::payload).and(documents::label),
        )
        .await
        .unwrap();
    assert_eq!(updated.id, inserted.id);
    assert_eq!(updated.version, inserted.version + 1);
}
