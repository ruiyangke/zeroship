use super::fixtures::CollectionFixture;
use super::*;

posts_schema!(pub exists_schema, readings);
use exists_schema::readings;

schema! {
    pub hidden_schema {
        hidden {
            #[orm(primary_key, readable = false, projectable = false)]
            id: Text,
            #[orm(readable = false, projectable = false)]
            payload: Bytes,
        }
    }
}

async fn exercise(db: &Database) {
    let entity = db.entity::<readings::Entity>().unwrap();
    assert!(!entity.exists(Filter::all()).await.unwrap());
    db.collection("readings")
        .unwrap()
        .insert(value!({"title":"ready"}))
        .await
        .unwrap();
    assert!(entity
        .exists(readings::title.eq("ready").unwrap())
        .await
        .unwrap());
    assert!(!entity
        .exists(readings::title.eq("absent").unwrap())
        .await
        .unwrap());
    assert!(entity
        .query()
        .order_by(readings::title.desc())
        .offset(100)
        .unwrap()
        .limit(1)
        .unwrap()
        .exists()
        .await
        .unwrap());

    db.collection("readings")
        .unwrap()
        .delete(value!({"title":"ready"}))
        .await
        .unwrap();
    assert!(!entity.exists(Filter::all()).await.unwrap());
    assert!(entity.query().include_deleted().exists().await.unwrap());

    let rollback = db
        .transaction(|tx| async move {
            tx.collection("readings")?
                .insert(value!({"title":"rolled-back"}))
                .await?;
            assert!(
                tx.entity::<readings::Entity>()?
                    .exists(readings::title.eq("rolled-back")?)
                    .await?
            );
            Err::<(), _>(DbError::internal("rollback existence fixture"))
        })
        .await;
    assert!(rollback.is_err());
    assert!(!entity
        .exists(readings::title.eq("rolled-back").unwrap())
        .await
        .unwrap());

    let escaped = db
        .transaction(|tx| async move {
            tx.collection("readings")?
                .insert(value!({"title":"in-transaction"}))
                .await?;
            let entity = tx.entity::<readings::Entity>()?;
            assert!(entity.exists(readings::title.eq("in-transaction")?).await?);
            Ok::<_, DbError>(entity.query())
        })
        .await
        .unwrap();
    assert!(matches!(
        escaped.exists().await,
        Err(DbError::ValidationFailed {
            code: "transaction_scope_expired",
            ..
        })
    ));

    let stale = entity.query();
    let mut changed = readings::Entity::schema().fields().clone();
    changed["title"].max_length = Some(64);
    db.context
        .with(|| {
            crate::descriptor::install_collections(
                db.binding(),
                Schema::new([("readings".into(), CollectionSchema::new(changed))]),
            )
        })
        .unwrap();
    assert!(matches!(
        stale.exists().await,
        Err(DbError::Configuration {
            code: "orm_schema_mismatch",
            ..
        })
    ));
}

async fn hidden_fields(postgres: bool) {
    use hidden_schema::hidden;
    let migration = value!({
        "id":{"type":"string", "primaryKey":true, "required":true},
        "payload":{"type":"bytes", "required":true},
    });
    let owner = if postgres {
        CollectionFixture::postgres_from_table_definition(
            "hidden",
            migration,
            "id TEXT PRIMARY KEY, payload BYTEA NOT NULL",
        )
        .await
    } else {
        CollectionFixture::sqlite_from_table_definition(
            "hidden",
            migration,
            "id TEXT PRIMARY KEY, payload BLOB NOT NULL",
        )
        .await
    };
    owner
        .database
        .collection("hidden")
        .unwrap()
        .insert(value!({
            "id":"record", "payload":Value::Bytes(vec![0, 255]),
        }))
        .await
        .unwrap();
    let db = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        Schema::new([("hidden".into(), hidden::Entity::schema().clone())]),
    )
    .unwrap();
    let entity = db.entity::<hidden::Entity>().unwrap();
    assert!(entity
        .exists(hidden::id.eq("record").unwrap())
        .await
        .unwrap());
    assert!(!entity
        .exists(hidden::id.eq("missing").unwrap())
        .await
        .unwrap());
    owner.close().await;
}

#[compio::test]
async fn sqlite_typed_exists_needs_no_readable_fields() {
    hidden_fields(false).await;
}

#[compio::test]
async fn postgres_typed_exists_needs_no_readable_fields() {
    hidden_fields(true).await;
}

#[compio::test]
async fn sqlite_typed_exists_preserves_visibility_and_transaction_scope() {
    let owner = CollectionFixture::sqlite_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    exercise(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_typed_exists_preserves_visibility_and_transaction_scope() {
    let owner = CollectionFixture::postgres_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    exercise(&owner.database).await;
    owner.close().await;
}
