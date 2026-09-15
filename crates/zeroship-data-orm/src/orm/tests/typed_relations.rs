use super::fixtures::CollectionFixture;
use super::*;

include!("../../../tests/fixtures/relations_schema.rs");
relations_schema!(pub relation_schema);
use relation_schema::{authors, posts};

#[derive(Debug, PartialEq, FromRow)]
#[orm(entity = authors)]
struct Author {
    id: String,
    name: String,
    serial: i64,
    payload: Vec<u8>,
}

#[derive(Debug, PartialEq, FromRow)]
#[orm(entity = posts)]
struct Post {
    id: String,
    title: String,
    #[orm(column = "authorId")]
    author_id: Option<String>,
}

#[derive(Debug, PartialEq, FromRow)]
#[orm(entity = posts)]
struct Title {
    title: String,
}

struct WrongRelationName;
impl Relation for WrongRelationName {
    type Source = posts::Entity;
    type Target = authors::Entity;
    const NAME: &'static str = "editor";
    const FIELD: &'static str = "authorId";
    const TARGET_COLUMN: &'static str = "id";
}

struct WrongRelationField;
impl Relation for WrongRelationField {
    type Source = posts::Entity;
    type Target = authors::Entity;
    const NAME: &'static str = "author";
    const FIELD: &'static str = "authorHandle";
    const TARGET_COLUMN: &'static str = "handle";
}

struct WrongRelationTargetColumn;
impl Relation for WrongRelationTargetColumn {
    type Source = posts::Entity;
    type Target = authors::Entity;
    const NAME: &'static str = "author";
    const FIELD: &'static str = "authorId";
    const TARGET_COLUMN: &'static str = "handle";
}

async fn fixture(postgres: bool) -> CollectionFixture {
    let mut fixture = if postgres {
        CollectionFixture::postgres("authors", value!({"name":{"type":"string"}})).await
    } else {
        CollectionFixture::sqlite("authors", value!({"name":{"type":"string"}})).await
    };
    fixture
        .replace_from_migration(
            "authors",
            include_str!("../../../tests/fixtures/typed-relations-migration.json"),
        )
        .await;
    fixture
}

async fn seed(db: &Database) -> Author {
    let row = db
        .collection("authors")
        .unwrap()
        .insert(value!({
            "name":"Ada", "handle":"ada", "serial":9_007_199_254_740_993_i64,
            "payload":Value::Bytes(vec![0, 128, 255]),
        }))
        .await
        .unwrap();
    let author = decode_rows::<authors::Entity, Author>(row)
        .unwrap()
        .pop()
        .unwrap();
    for (title, id) in [
        ("first", Some(author.id.as_str())),
        ("second", Some(author.id.as_str())),
        ("unassigned", None),
    ] {
        db.collection("posts")
            .unwrap()
            .insert(value!({
                "title":title, "authorId":id, "authorHandle":id.map(|_| "ada"),
                "authorSerial":id.map(|_| author.serial),
            }))
            .await
            .unwrap();
    }
    author
}

async fn exercise(postgres: bool) {
    let owner = fixture(postgres).await;
    let db = &owner.database;
    let author = seed(db).await;
    let posts = db.entity::<posts::Entity>().unwrap();
    for result in [
        posts
            .query()
            .with_related(WrongRelationName)
            .all::<Post, Author>()
            .await,
        posts
            .query()
            .with_related(WrongRelationField)
            .all::<Post, Author>()
            .await,
        posts
            .query()
            .with_related(WrongRelationTargetColumn)
            .all::<Post, Author>()
            .await,
    ] {
        assert!(
            matches!(result.unwrap_err(), DbError::Configuration {code, ..} if code == "orm_schema_mismatch")
        );
    }

    let rows = posts
        .query()
        .with_related(posts::relations::author)
        .order_by(posts::title.asc())
        .all::<Post, Author>()
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].0.author_id.as_deref(), Some(author.id.as_str()));
    assert_eq!(rows[0].1.as_ref(), Some(&author));
    assert_eq!(rows[1].1.as_ref(), Some(&author));
    assert_eq!(rows[2].0.author_id, None);
    assert_eq!(rows[2].1, None);

    assert_eq!(
        posts
            .query()
            .with_related(posts::relations::author)
            .limit(1)
            .unwrap()
            .offset(100)
            .unwrap()
            .count()
            .await
            .unwrap(),
        3
    );
    assert!(!posts
        .query()
        .with_related(posts::relations::author)
        .filter(posts::title.eq("absent").unwrap())
        .exists()
        .await
        .unwrap());

    let paged = posts
        .query()
        .order_by(posts::title.asc())
        .offset(1)
        .unwrap()
        .limit(1)
        .unwrap()
        .with_related(posts::relations::author)
        .all::<Title, Author>()
        .await
        .unwrap();
    assert_eq!(
        paged,
        vec![(
            Title {
                title: "second".into()
            },
            Some(author)
        )]
    );
    for row in [
        posts
            .query()
            .filter(posts::title.eq("first").unwrap())
            .with_related(posts::relations::authorByHandle)
            .first::<Title, Author>()
            .await
            .unwrap(),
        posts
            .query()
            .filter(posts::title.eq("first").unwrap())
            .with_related(posts::relations::authorBySerial)
            .first::<Title, Author>()
            .await
            .unwrap(),
    ] {
        let (post, author) = row.unwrap();
        assert_eq!(post.title, "first");
        assert_eq!(author.unwrap().serial, 9_007_199_254_740_993_i64);
    }
    assert!(posts
        .query()
        .filter(posts::title.eq("absent").unwrap())
        .with_related(posts::relations::author)
        .first::<Post, Author>()
        .await
        .unwrap()
        .is_none());

    db.collection("authors")
        .unwrap()
        .delete(value!({"handle":"ada"}))
        .await
        .unwrap();
    let rows = posts
        .query()
        .include_deleted()
        .with_related(posts::relations::author)
        .all::<Post, Author>()
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().all(|(_, author)| author.is_none()),
        "parent visibility must not expose deleted targets"
    );
    owner.close().await;
}

#[compio::test]
async fn typed_relations_sqlite() {
    exercise(false).await;
}

#[compio::test]
async fn locking_reads_refuse_relation_loading() {
    let owner = fixture(false).await;
    seed(&owner.database).await;
    owner
        .database
        .transaction(|tx| async move {
            let posts = tx.entity::<posts::Entity>()?;
            let refused = posts
                .query()
                .for_update()?
                .with_related(posts::relations::author)
                .all::<Post, Author>()
                .await
                .unwrap_err();
            assert!(
                matches!(&refused, DbError::ValidationFailed { code, .. } if *code == "invalid_read"),
                "{refused:?}"
            );
            // Control: the same relation load without a lock reads in the same transaction.
            let loaded = posts
                .query()
                .with_related(posts::relations::author)
                .all::<Post, Author>()
                .await?;
            assert_eq!(loaded.len(), 3);
            Ok(())
        })
        .await
        .unwrap();
    owner.close().await;
}

#[compio::test]
async fn typed_relations_postgres() {
    exercise(true).await;
}

async fn scope_and_schema(postgres: bool) {
    let owner = fixture(postgres).await;
    let db = &owner.database;
    seed(db).await;
    let capture = crate::cdc::read_set::Capture::new(true);
    let prepared = db.context.with(|| {
        capture.with(|| {
            db.entity::<posts::Entity>()
                .unwrap()
                .query()
                .with_related(posts::relations::author)
                .all::<Post, Author>()
        })
    });
    let reads = capture.snapshot();
    assert!(reads.iter().any(|read| read.collection == "posts"));
    assert!(reads.iter().any(|read| read.collection == "authors"));
    assert_eq!(prepared.await.unwrap().len(), 3);
    let mut unrelated_schema = authors::Entity::schema().fields().clone();
    unrelated_schema["name"].readable = false;
    let unrelated = Database::from_schema(
        db.binding.clone(),
        db.backend.clone(),
        Schema::new([
            ("posts".into(), posts::Entity::schema().clone()),
            ("authors".into(), CollectionSchema::new(unrelated_schema)),
        ]),
    )
    .unwrap();
    let prepared = db
        .entity::<posts::Entity>()
        .unwrap()
        .query()
        .with_related(posts::relations::author)
        .all::<Post, Author>();
    assert_eq!(unrelated.context.scope(prepared).await.unwrap().len(), 3);

    let rolled_back = db
        .transaction(|tx| async move {
            let author = decode_rows::<authors::Entity, Author>(
                tx.collection("authors")?
                    .insert(value!({
                        "name":"Grace", "handle":"grace", "serial":9_007_199_254_740_995_i64,
                        "payload":Value::Bytes(vec![1, 2]),
                    }))
                    .await?,
            )?
            .pop()
            .unwrap();
            tx.collection("posts")?
                .insert(value!({"title":"uncommitted", "authorId":author.id}))
                .await?;
            let related = tx
                .entity::<posts::Entity>()?
                .query()
                .filter(posts::title.eq("uncommitted")?)
                .with_related(posts::relations::author)
                .first::<Post, Author>()
                .await?
                .unwrap();
            assert_eq!(related.1.unwrap().name, "Grace");
            Err::<(), _>(DbError::validation(
                "rollback_relation",
                "rollback relation fixture",
            ))
        })
        .await;
    assert!(
        matches!(rolled_back.unwrap_err(), DbError::ValidationFailed {code, ..} if code == "rollback_relation")
    );
    assert_eq!(
        db.entity::<posts::Entity>()
            .unwrap()
            .count(Filter::all())
            .await
            .unwrap(),
        3
    );
    let escaped = db
        .transaction(|tx| async move {
            let posts = tx.entity::<posts::Entity>()?;
            let rows = posts
                .query()
                .with_related(posts::relations::author)
                .all::<Post, Author>()
                .await?;
            assert_eq!(rows.len(), 3);
            let prepared = posts
                .query()
                .with_related(posts::relations::author)
                .all::<Post, Author>();
            Ok((
                posts.query().with_related(posts::relations::author),
                prepared,
            ))
        })
        .await
        .unwrap();
    for result in [escaped.0.all::<Post, Author>().await, escaped.1.await] {
        assert!(
            matches!(result.unwrap_err(), DbError::ValidationFailed {code, ..} if code == "transaction_scope_expired")
        );
    }

    let query = db
        .entity::<posts::Entity>()
        .unwrap()
        .query()
        .with_related(posts::relations::author);
    let prepared = db
        .entity::<posts::Entity>()
        .unwrap()
        .query()
        .with_related(posts::relations::author)
        .all::<Post, Author>();
    let mut changed = authors::Entity::schema().fields().clone();
    changed["name"].readable = false;
    db.context
        .with(|| {
            crate::descriptor::install_collections(
                &db.binding,
                Schema::new([
                    ("posts".into(), posts::Entity::schema().clone()),
                    ("authors".into(), CollectionSchema::new(changed)),
                ]),
            )
        })
        .unwrap();
    assert!(
        matches!(query.all::<Post, Author>().await.unwrap_err(), DbError::Configuration {code, ..} if code == "orm_schema_mismatch")
    );
    assert!(
        matches!(prepared.await.unwrap_err(), DbError::Configuration {code, ..} if code == "orm_schema_mismatch")
    );
    owner.close().await;
}

#[compio::test]
async fn typed_relations_sqlite_scope_and_schema() {
    scope_and_schema(false).await;
}

#[compio::test]
async fn typed_relations_postgres_scope_and_schema() {
    scope_and_schema(true).await;
}

async fn case_insensitive_key(postgres: bool) {
    let mut owner = fixture(postgres).await;
    let mut migration: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/typed-relations-migration.json"
    ))
    .unwrap();
    for column in migration["ops"][0]["columns"].as_array_mut().unwrap() {
        if column["name"] == "handle" {
            column["caseSensitive"] = false.into();
        }
    }
    for column in migration["ops"][1]["columns"].as_array_mut().unwrap() {
        if column["name"] == "authorHandle" {
            column["caseSensitive"] = false.into();
        }
    }
    owner.install_case_insensitive_text().await;
    owner
        .replace_from_migration("authors", &migration.to_string())
        .await;
    let db = &owner.database;
    db.collection("authors")
        .unwrap()
        .insert(value!({
            "name":"Ada", "handle":"Ada", "serial":1_i64, "payload":Value::Bytes(vec![255]),
        }))
        .await
        .unwrap();
    for (title, handle) in [("first", "Ada"), ("second", "ada")] {
        db.collection("posts")
            .unwrap()
            .insert(value!({"title":title, "authorHandle":handle}))
            .await
            .unwrap();
    }
    let Output::Rows { rows, .. } = db
        .collection("posts")
        .unwrap()
        .find(
            value!({}),
            value!({"orderBy":{"title":1}, "with":{"authorByHandle":true}}),
        )
        .await
        .unwrap()
    else {
        panic!("expected related rows")
    };
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["authorByHandle"]["name"], value!("Ada"));
        assert_eq!(row["authorByHandle"]["handle"], value!("Ada"));
        assert_eq!(row["authorByHandle"]["payload"], Value::Bytes(vec![255]));
        assert_eq!(
            row.as_object()
                .unwrap()
                .keys()
                .filter(|name| name.starts_with("_relation"))
                .count(),
            0
        );
    }
    owner.close().await;
}

#[compio::test]
async fn related_keys_use_sqlite_case_insensitive_equality() {
    case_insensitive_key(false).await;
}

#[compio::test]
async fn related_keys_use_postgres_case_insensitive_equality() {
    case_insensitive_key(true).await;
}

#[compio::test]
async fn empty_related_reads_validate_the_target_projection_budget() {
    let owner = fixture(false).await;
    let db = &owner.database;
    let mut author_schema = authors::Entity::schema().fields().clone();
    for index in 0..read::MAX_READ_FIELDS {
        author_schema.insert(
            format!("extra{index}"),
            ColumnSchema::new(LogicalType::Text),
        );
    }
    db.context
        .with(|| {
            crate::descriptor::install_collections(
                &db.binding,
                Schema::new([
                    ("posts".into(), posts::Entity::schema().clone()),
                    ("authors".into(), CollectionSchema::new(author_schema)),
                ]),
            )
        })
        .unwrap();
    let result = db
        .collection("posts")
        .unwrap()
        .find(value!({}), value!({"with":{"author":true}}))
        .await;
    assert!(
        matches!(result, Err(DbError::ValidationFailed { message, .. }) if message.contains("projection or parameter budget"))
    );
    owner.close().await;
}

#[compio::test]
async fn related_null_aliases_obey_the_final_result_budget() {
    let owner = fixture(false).await;
    let db = &owner.database;
    let (relation, route) = db.context.with(|| {
        let route = db.capture_route();
        let relation = crate::orm::relations::FindRelations::new(
            &db.binding,
            route.sql_registration(),
            "posts",
            &mut value!({"with":{"authorByHandle":true}}),
        )
        .unwrap()
        .unwrap();
        (relation, route.bind(db.backend.clone()).unwrap())
    });
    let mut row = value!({"title":"", "authorHandle":null});
    let mut remaining = read::MAX_READ_RESULT_BYTES;
    read::consume_budget(&row, &mut remaining).unwrap();
    row["title"] = "x".repeat(remaining).into();
    let mut check = read::MAX_READ_RESULT_BYTES;
    read::consume_budget(&row, &mut check).unwrap();
    assert_eq!(check, 0);
    let mut result = crate::crud::read_pipeline::ApplyResult {
        rows: vec![row],
        has_masked: false,
    };
    let outcome = db
        .context
        .scope(relation.apply(&db.binding, &route, &mut result))
        .await;
    assert!(
        matches!(outcome, Err(DbError::ValidationFailed { message, .. }) if message.contains("size budget"))
    );
    owner.close().await;
}
