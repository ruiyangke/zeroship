#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use thread-local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;

#[expect(clippy::too_many_lines, reason = "shared backend conformance scenario")]
async fn exercise(owner: CollectionFixture) {
    let db = &owner.database;
    let table = db.entity::<posts::Entity>().unwrap();
    for title in ["alpha", "beta", "gamma"] {
        table
            .insert::<_, Post>(NewPost {
                title: title.into(),
            })
            .await
            .unwrap();
    }
    table
        .update::<_, Post>(
            posts::title.eq("alpha").unwrap(),
            posts::nickname.set(None::<&str>).unwrap(),
        )
        .await
        .unwrap();
    let p = table.alias("p").unwrap();
    let q = table.alias("q").unwrap();
    let joined = || {
        db.from(&p)
            .inner_join(
                &q,
                p.column(posts::counter)
                    .eq(q.column(posts::counter))
                    .unwrap(),
            )
            .unwrap()
    };
    assert_eq!(
        joined().having(ReadPredicate::all()).count().await.unwrap(),
        9
    );
    assert_eq!(
        joined()
            .limit(1)
            .unwrap()
            .offset(100)
            .unwrap()
            .count()
            .await
            .unwrap(),
        9
    );
    assert!(joined()
        .limit(1)
        .unwrap()
        .offset(100)
        .unwrap()
        .exists()
        .await
        .unwrap());
    assert_eq!(
        joined()
            .filter(p.column(posts::title).eq("absent").unwrap())
            .count()
            .await
            .unwrap(),
        0
    );
    assert!(!joined()
        .filter(p.column(posts::title).eq("absent").unwrap())
        .exists()
        .await
        .unwrap());
    let first = db
        .from(&p)
        .order_by(p.column(posts::nickname).asc().nulls_first())
        .order_by(p.column(posts::title).asc())
        .select(p.row::<Post>())
        .unwrap()
        .first()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.title, "alpha");
    let second = db
        .from(&p)
        .order_by(p.column(posts::title).asc())
        .offset(1)
        .unwrap()
        .select(p.row::<Post>())
        .unwrap()
        .first()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.title, "beta");
    let grouped = || {
        joined()
            .group_by(p.column(posts::title))
            .having(count_rows().gte(3_i64).unwrap())
    };
    assert_eq!(grouped().limit(1).unwrap().count().await.unwrap(), 3);
    assert!(grouped().exists().await.unwrap());
    assert_eq!(
        grouped()
            .having(count_rows().lt(3_i64).unwrap())
            .count()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        grouped()
            .select((p.column(posts::title).select::<String>(), count_rows()))
            .unwrap()
            .count()
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        joined()
            .select(count_rows())
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        joined()
            .having(count_rows().gte(9_i64).unwrap())
            .count()
            .await
            .unwrap(),
        1
    );
    assert!(!joined()
        .having(count_rows().gt(9_i64).unwrap())
        .exists()
        .await
        .unwrap());
    assert_eq!(
        table
            .query()
            .filter(posts::title.eq("alpha").unwrap())
            .filter(posts::title.eq("beta").unwrap())
            .count()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        db.from(&p)
            .filter(p.column(posts::title).eq("alpha").unwrap())
            .filter(p.column(posts::title).eq("beta").unwrap())
            .count()
            .await
            .unwrap(),
        0
    );
    table
        .delete::<Post>(posts::title.eq("beta").unwrap())
        .await
        .unwrap();
    let visible = table.alias("visible").unwrap();
    let deleted = table.alias("deleted").unwrap().include_deleted();
    assert_eq!(
        db.from(&visible)
            .inner_join(
                &deleted,
                visible
                    .column(posts::counter)
                    .eq(deleted.column(posts::counter))
                    .unwrap()
            )
            .unwrap()
            .count()
            .await
            .unwrap(),
        6
    );
    assert_eq!(db.from(&deleted).count().await.unwrap(), 3);
    owner.close().await;
}
#[compio::test]
async fn typed_read_terminals_sqlite() {
    Box::pin(exercise(
        CollectionFixture::sqlite_native(
            "posts",
            posts::Entity::schema().clone(),
            fixtures::post_migration_fields(),
        )
        .await,
    ))
    .await;
}
#[compio::test]
async fn typed_read_terminals_postgres() {
    Box::pin(exercise(
        CollectionFixture::postgres_native(
            "posts",
            posts::Entity::schema().clone(),
            fixtures::post_migration_fields(),
        )
        .await,
    ))
    .await;
}

async fn exercise_chained_predicates(owner: CollectionFixture) {
    let db = &owner.database;
    let table = db.entity::<posts::Entity>().unwrap();
    table
        .insert::<_, Post>(NewPost {
            title: "present".into(),
        })
        .await
        .unwrap();
    let p = table.alias("p").unwrap();
    let mut filtered = db.from(&p);
    let mut grouped = db.from(&p).group_by(p.column(posts::title));
    for _ in 0..=crate::sql::MAX_PREDICATE_DEPTH {
        filtered = filtered.filter(p.column(posts::title).eq("present").unwrap());
        grouped = grouped.having(count_rows().eq(1_i64).unwrap());
    }
    let results = [filtered.count().await, grouped.count().await];
    assert!(
        results.iter().all(|result| matches!(result, Ok(1))),
        "{results:?}"
    );

    let mut nested = p.column(posts::title).eq("present").unwrap();
    for _ in 0..=crate::sql::MAX_PREDICATE_DEPTH {
        nested = nested.negate();
    }
    assert!(db.from(&p).filter(nested).count().await.is_err());
    owner.close().await;
}

#[compio::test]
async fn typed_chained_predicates_sqlite() {
    Box::pin(exercise_chained_predicates(
        CollectionFixture::sqlite_native(
            "posts",
            posts::Entity::schema().clone(),
            fixtures::post_migration_fields(),
        )
        .await,
    ))
    .await;
}

#[compio::test]
async fn typed_chained_predicates_postgres() {
    Box::pin(exercise_chained_predicates(
        CollectionFixture::postgres_native(
            "posts",
            posts::Entity::schema().clone(),
            fixtures::post_migration_fields(),
        )
        .await,
    ))
    .await;
}
