use super::*;
use zeroship_data_core::{encryption::LocalKeySource, storage::SqlExecutor};
use zeroship_data_query_builder::value;

#[derive(Debug)]
struct Post {
    id: String,
    title: String,
    version: i64,
}
struct NewPost {
    title: String,
}
impl Model for Post {
    const COLLECTION: &'static str = "posts";
    type Insert = NewPost;
    fn from_row(mut row: Row) -> Result<Self, DbError> {
        Ok(Self {
            id: row.take("id")?,
            title: row.take("title")?,
            version: row.take("version")?,
        })
    }
}

impl EncodeRecord for NewPost {
    fn into_record(self) -> Record {
        [("title".into(), self.title.into())].into()
    }
}
impl Post {
    const ID: Field<Self, String> = Field::new("id");
    const TITLE: Field<Self, String> = Field::new("title");
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
        value!({ "title": { "type": "string", "required": true }, "payload": {"type":"bytes"} });
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
            &serde_json::to_value(&schema).unwrap(),
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
        .insert(NewPost {
            title: "hello".into(),
        })
        .await
        .unwrap();
    assert!(inserted.id.starts_with("post_"));
    let updated = posts
        .update(
            Post::ID.eq(inserted.id.clone()),
            Post::TITLE.set("edited".into()),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.title, "edited");
    assert!(updated.version > inserted.version);
    assert_eq!(
        posts
            .find(Filter::all(), FindOptions::default())
            .await
            .unwrap()
            .len(),
        1
    );
    posts
        .delete(Post::ID.eq(inserted.id.clone()))
        .await
        .unwrap()
        .unwrap();
    assert!(posts
        .find(Filter::all(), FindOptions::default())
        .await
        .unwrap()
        .is_empty());
    let collection = db.collection("posts").unwrap();
    assert_eq!(
        count(
            collection
                .count(value!({}), value!({"include_deleted":true}))
                .await
                .unwrap()
        ),
        1
    );
    collection
        .execute(Operation::Restore {
            filter: value!({"id": inserted.id}),
            many: false,
        })
        .await
        .unwrap();
    assert_eq!(
        posts
            .find(Filter::all(), FindOptions::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[compio::test]
async fn transactions_commit_rollback_and_expire_escaped_collections() {
    let (db, _directory) = database().await;
    let escaped = db
        .transaction(|tx| async move {
            let posts = tx.collection("posts")?;
            posts.insert(value!({"title":"committed"})).await?;
            Ok(posts)
        })
        .await
        .unwrap();
    assert!(escaped
        .find(value!({}), value!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("settled"));
    let result: Result<(), DbError> = db
        .transaction(|tx| async move {
            tx.collection("posts")?
                .insert(value!({"title":"rolled back"}))
                .await?;
            Err(DbError::internal("callback failed"))
        })
        .await;
    assert!(result.is_err());
    let posts = db
        .model::<Post>()
        .unwrap()
        .find(Filter::all(), FindOptions::default())
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
            .insert(value!({"title":"outer"}))
            .await?;
        let nested: Result<(), DbError> = tx
            .transaction(|nested| async move {
                nested
                    .collection("posts")?
                    .insert(value!({"title":"inner"}))
                    .await?;
                Err(DbError::internal("nested callback failed"))
            })
            .await;
        assert!(nested.is_err());
        assert_eq!(
            count(
                tx.collection("posts")?
                    .count(value!({}), value!({}))
                    .await?
            ),
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
                .count(value!({}), value!({}))
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
            posts.insert(value!({"title":"duplicate"})).await?;
            assert!(posts.insert(value!({"title":"duplicate"})).await.is_err());
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
                .count(value!({}), value!({}))
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
            filter: value!({}),
            options: value!({}),
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
            .insert(value!({"title":title,"payload": Value::Bytes(vec![0, 1, 2, 255])}))
            .await
            .unwrap();
        let Output::Rows { rows, .. } = inserted else {
            panic!("expected rows")
        };
        assert_eq!(rows[0]["title"], title);
        let Output::Rows { rows, .. } = posts
            .find(value!({"id": rows[0]["id"]}), value!({}))
            .await
            .unwrap()
        else {
            panic!("expected rows")
        };
        assert_eq!(rows[0]["title"], title);
        assert_eq!(rows[0]["payload"], Value::Bytes(vec![0, 1, 2, 255]));
    }
}
