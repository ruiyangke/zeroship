use super::fixtures::CollectionFixture;
use super::*;

type Pending = std::pin::Pin<Box<dyn Future<Output = Result<(), DbError>>>>;

fn discard<T: 'static>(future: impl Future<Output = Result<T, DbError>> + 'static) -> Pending {
    Box::pin(async move { future.await.map(|_| ()) })
}

async fn exercise(db: &Database) {
    let entity = db.entity::<posts::Entity>().unwrap();
    let _: Post = entity
        .insert(NewPost {
            title: "original".into(),
        })
        .await
        .unwrap();
    let alias = entity.alias("p").unwrap();
    let pending = vec![
        ("all", discard(entity.query().all::<Post>())),
        ("first", discard(entity.query().first::<Post>())),
        (
            "find",
            discard(entity.find::<Post>(Filter::all(), FindOptions::default())),
        ),
        ("count", discard(entity.count(Filter::all()))),
        ("exists", discard(entity.exists(Filter::all()))),
        (
            "aliased rows",
            discard(db.from(&alias).select(alias.row::<Post>()).unwrap().all()),
        ),
        (
            "aliased count",
            discard(db.from(&alias).select(count_rows()).unwrap().all()),
        ),
        (
            "grouped count",
            discard(
                db.from(&alias)
                    .group_by(alias.column(posts::title))
                    .select(count_rows())
                    .unwrap()
                    .all(),
            ),
        ),
        (
            "insert",
            discard(entity.insert::<_, Post>(NewPost {
                title: "inserted".into(),
            })),
        ),
        (
            "insert_many",
            discard(entity.insert_many::<_, Post>([NewPost {
                title: "batch".into(),
            }])),
        ),
        (
            "update",
            discard(entity.update::<_, Post>(Filter::all(), posts::title.set("updated").unwrap())),
        ),
        (
            "update_many",
            discard(entity.update_many(Filter::all(), posts::counter.set(99_i64).unwrap())),
        ),
        ("delete", discard(entity.delete::<Post>(Filter::all()))),
        ("delete_many", discard(entity.delete_many(Filter::all()))),
        ("restore", discard(entity.restore::<Post>(Filter::all()))),
        ("restore_many", discard(entity.restore_many(Filter::all()))),
        ("purge", discard(entity.purge::<Post>(Filter::all()))),
        ("purge_many", discard(entity.purge_many(Filter::all()))),
        (
            "upsert",
            discard(entity.upsert::<_, Post>(
                NewPost {
                    title: "upserted".into(),
                },
                ConflictTarget::new(posts::title),
            )),
        ),
    ];
    let mut changed = posts::Entity::schema().fields().clone();
    changed["title"].max_length = Some(64);
    db.context
        .with(|| {
            crate::descriptor::install_collections(
                db.binding(),
                Schema::new([("posts".into(), CollectionSchema::new(changed))]),
            )
        })
        .unwrap();
    let mut failures = Vec::new();
    for (method, future) in pending {
        let result = future.await;
        if !matches!(
            result,
            Err(DbError::Configuration {
                code: "orm_schema_mismatch",
                ..
            })
        ) {
            failures.push(format!("{method}: {result:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    db.context
        .with(|| {
            crate::descriptor::install_collections(
                db.binding(),
                Schema::new([("posts".into(), posts::Entity::schema().clone())]),
            )
        })
        .unwrap();
    let remaining = entity
        .query()
        .include_deleted()
        .all::<Post>()
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].title, "original");
    assert_eq!(remaining[0].version, 1);
}

#[compio::test]
async fn sqlite_typed_operations_recheck_schema_before_execution() {
    let owner = CollectionFixture::sqlite_native(
        "posts",
        posts::Entity::schema().clone(),
        fixtures::post_migration_fields(),
    )
    .await;
    exercise(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_typed_operations_recheck_schema_before_execution() {
    let owner = CollectionFixture::postgres_native(
        "posts",
        posts::Entity::schema().clone(),
        fixtures::post_migration_fields(),
    )
    .await;
    exercise(&owner.database).await;
    owner.close().await;
}
