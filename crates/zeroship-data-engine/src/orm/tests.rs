use super::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroship_data_core::{encryption::LocalKeySource, storage::SqlExecutor};

#[derive(Debug, Deserialize)]
struct Post {
    id: String,
    title: String,
    version: i64,
}
#[derive(Serialize)]
struct NewPost {
    title: String,
}
impl Model for Post {
    const COLLECTION: &'static str = "posts";
    type Insert = NewPost;
}

async fn database() -> (Database, tempfile::TempDir) {
    crate::reset_engine_for_tests();
    let directory = tempfile::tempdir().unwrap();
    let backend = Rc::new(
        crate::backend_selection::open_sqlite_backend(
            directory.path().join("control.sqlite"),
            LocalKeySource::EnvVar,
        )
        .await
        .unwrap(),
    );
    let binding = DbBinding::cold_start("orm_fixture");
    backend.attach_app_file(binding.app_id()).await.unwrap();
    let schema =
        json!({ "title": { "type": "string", "required": true }, "payload": {"type":"bytes"} });
    let policy =
        zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined([7u8; 32], 1)
            .unwrap()
            .current_ceiling_for_app(&uuid::Uuid::nil(), None)
            .unwrap()
            .policy;
    let statements =
        zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect_scoped_statements(
            zeroship_migrate::shipping_vendors(),
            binding.schema().as_str(),
            "posts",
            &schema,
            &zeroship_migrate::schema::query::FkEmission::Inline,
            &zeroship_migrate_sqlite::DIALECT,
            false,
            &policy,
        )
        .unwrap();
    for sql in statements {
        backend.pool_exec(&sql, &[]).await.unwrap();
    }
    backend
        .pool_exec(
            "CREATE UNIQUE INDEX orm_fixture.unique_title ON posts(title)",
            &[],
        )
        .await
        .unwrap();
    let database = Database::from_schema(
        binding,
        BackendHandle::Sqlite(backend),
        vec![("posts".into(), schema)],
    )
    .unwrap();
    (database, directory)
}

fn count(output: Output) -> i64 {
    match output {
        Output::Count(n) => n,
        other => panic!("expected count: {other:?}"),
    }
}

#[compio::test]
async fn mapped_models_use_the_migration_schema_and_orm_lifecycle() {
    let (db, _directory) = database().await;
    assert!(db.collection("not_declared").is_err());
    let posts = db.model::<Post>().unwrap();
    let inserted = posts
        .insert(&NewPost {
            title: "hello".into(),
        })
        .await
        .unwrap();
    assert!(inserted.id.starts_with("post_"));
    let updated = posts
        .update(json!({"id": inserted.id}), json!({"title": "edited"}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.title, "edited");
    assert!(updated.version > inserted.version);
    assert_eq!(posts.find(json!({}), json!({})).await.unwrap().len(), 1);
    posts
        .delete(json!({"id": inserted.id}))
        .await
        .unwrap()
        .unwrap();
    assert!(posts.find(json!({}), json!({})).await.unwrap().is_empty());
    let collection = db.collection("posts").unwrap();
    assert_eq!(
        count(
            collection
                .count(json!({}), json!({"include_deleted":true}))
                .await
                .unwrap()
        ),
        1
    );
    collection
        .execute(Operation::Restore {
            filter: json!({"id": inserted.id}),
            many: false,
        })
        .await
        .unwrap();
    assert_eq!(posts.find(json!({}), json!({})).await.unwrap().len(), 1);
}

#[compio::test]
async fn transactions_commit_rollback_and_expire_escaped_collections() {
    let (db, _directory) = database().await;
    let escaped = db
        .transaction(|tx| async move {
            let posts = tx.collection("posts")?;
            posts.insert(json!({"title":"committed"})).await?;
            Ok(posts)
        })
        .await
        .unwrap();
    assert!(escaped
        .find(json!({}), json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("settled"));
    let result: Result<(), DbError> = db
        .transaction(|tx| async move {
            tx.collection("posts")?
                .insert(json!({"title":"rolled back"}))
                .await?;
            Err(DbError::internal("callback failed"))
        })
        .await;
    assert!(result.is_err());
    let posts = db
        .model::<Post>()
        .unwrap()
        .find(json!({}), json!({}))
        .await
        .unwrap();
    assert_eq!(
        posts.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(),
        ["committed"]
    );
}

#[compio::test]
async fn nested_callback_failure_rolls_back_its_savepoint() {
    let (db, _directory) = database().await;
    db.transaction(|tx| async move {
        tx.collection("posts")?
            .insert(json!({"title":"outer"}))
            .await?;
        let nested: Result<(), DbError> = tx
            .transaction(|nested| async move {
                nested
                    .collection("posts")?
                    .insert(json!({"title":"inner"}))
                    .await?;
                Err(DbError::internal("nested callback failed"))
            })
            .await;
        assert!(nested.is_err());
        assert_eq!(
            count(tx.collection("posts")?.count(json!({}), json!({})).await?),
            1
        );
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        count(
            db.collection("posts")
                .unwrap()
                .count(json!({}), json!({}))
                .await
                .unwrap()
        ),
        1
    );
}

#[compio::test]
async fn caught_statement_failure_cannot_commit_a_poisoned_transaction() {
    let (db, _directory) = database().await;
    let result = db
        .transaction(|tx| async move {
            let posts = tx.collection("posts")?;
            posts.insert(json!({"title":"duplicate"})).await?;
            assert!(posts.insert(json!({"title":"duplicate"})).await.is_err());
            Ok(())
        })
        .await;
    assert!(
        result.is_err(),
        "a caught SQL error must still prevent commit"
    );
    assert_eq!(
        count(
            db.collection("posts")
                .unwrap()
                .count(json!({}), json!({}))
                .await
                .unwrap()
        ),
        0
    );
}

#[compio::test]
async fn preparation_rejects_a_route_for_another_database() {
    let (db, _directory) = database().await;
    let route = CapturedRoute::pool_for_tests("another_app", crate::compile::SqlDialect::Sqlite);
    let result = PreparedOperation::new(
        db.binding.clone(),
        "posts",
        route,
        None,
        Operation::Find {
            filter: json!({}),
            options: json!({}),
        },
    );
    assert!(result.is_err());
}

#[compio::test]
async fn binary_columns_round_trip_without_reinterpreting_text() {
    let (db, _directory) = database().await;
    let posts = db.collection("posts").unwrap();
    for title in ["__zsbin_blob__:aGVsbG8=", "__zsbin_blob__:not base64"] {
        let inserted = posts
            .insert(json!({"title":title,"payload":"AAEC/w=="}))
            .await
            .unwrap();
        let Output::Rows { rows, .. } = inserted else {
            panic!("expected rows")
        };
        assert_eq!(rows[0]["title"], title);
        let Output::Rows { rows, .. } = posts
            .find(json!({"id": rows[0]["id"]}), json!({}))
            .await
            .unwrap()
        else {
            panic!("expected rows")
        };
        assert_eq!(rows[0]["title"], title);
        assert_eq!(rows[0]["payload"], "AAEC/w==");
    }
}
