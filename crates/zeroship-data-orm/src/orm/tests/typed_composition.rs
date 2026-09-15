#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use thread-local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;

#[compio::test]
async fn typed_predicates_and_ordering_retain_their_database() {
    let local = CollectionFixture::sqlite_native(
        "posts",
        posts::Entity::schema().clone(),
        fixtures::post_migration_fields(),
    )
    .await;
    let foreign = CollectionFixture::sqlite_native(
        "posts",
        posts::Entity::schema().clone(),
        fixtures::post_migration_fields(),
    )
    .await;
    let db = &local.database;
    let p = db.entity::<posts::Entity>().unwrap().alias("p").unwrap();
    let other = foreign
        .database
        .entity::<posts::Entity>()
        .unwrap()
        .alias("p")
        .unwrap();
    let filtered = async {
        db.from(&p)
            .filter(
                p.column(posts::title)
                    .eq("local")?
                    .or(other.column(posts::title).eq("ready")?),
            )
            .select(count_rows())?
            .all()
            .await
    }
    .await;
    let compared = async {
        db.from(&p)
            .filter(p.column(posts::title).eq(other.column(posts::title))?)
            .select(count_rows())?
            .all()
            .await
    }
    .await;
    let grouped = async {
        db.from(&p)
            .having(other.column(posts::counter).sum().gte(1_i64)?)
            .select(count_rows())?
            .all()
            .await
    }
    .await;
    let ordered = async {
        db.from(&p)
            .order_by(other.column(posts::title).asc())
            .select(p.row::<Post>())?
            .all()
            .await
    }
    .await;
    for result in [
        filtered.map(|_| ()),
        compared.map(|_| ()),
        ordered.map(|_| ()),
        grouped.map(|_| ()),
    ] {
        assert!(
            matches!(
                result,
                Err(DbError::ValidationFailed {
                    code: "invalid_read",
                    ..
                })
            ),
            "{result:?}"
        );
    }
    local.close().await;
    foreign.close().await;
}

#[compio::test]
async fn typed_predicates_and_ordering_retain_their_transaction() {
    let owner = CollectionFixture::sqlite_native(
        "posts",
        posts::Entity::schema().clone(),
        fixtures::post_migration_fields(),
    )
    .await;
    let db = &owner.database;
    let p = db.entity::<posts::Entity>().unwrap().alias("p").unwrap();
    let (predicate, order) = db
        .transaction(|tx| async move {
            let p = tx.entity::<posts::Entity>()?.alias("p")?;
            Ok::<_, DbError>((
                p.column(posts::title).eq("ready")?,
                p.column(posts::title).asc(),
            ))
        })
        .await
        .unwrap();
    let filtered = async {
        db.from(&p)
            .filter(predicate)
            .select(count_rows())?
            .all()
            .await
    }
    .await;
    let ordered = async {
        db.from(&p)
            .order_by(order)
            .select(p.row::<Post>())?
            .all()
            .await
    }
    .await;
    for result in [filtered.map(|_| ()), ordered.map(|_| ())] {
        assert!(
            matches!(
                result,
                Err(DbError::ValidationFailed {
                    code: "transaction_scope_expired",
                    ..
                })
            ),
            "{result:?}"
        );
    }
    let deferred = db
        .transaction(|tx| async move {
            let inside = tx.entity::<posts::Entity>()?.alias("p")?;
            Ok::<_, DbError>(db
                .from(&p)
                .filter(inside.column(posts::title).eq("ready")?)
                .select(count_rows())?
                .all())
        })
        .await
        .unwrap();
    assert!(matches!(
        deferred.await,
        Err(DbError::ValidationFailed {
            code: "transaction_scope_expired",
            ..
        })
    ));
    owner.close().await;
}

async fn compare_values_and_expressions(owner: CollectionFixture) {
    let db = &owner.database;
    let table = db.entity::<posts::Entity>().unwrap();
    for title in ["alpha", "beta"] {
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
            posts::nickname.set(Some("alpha")).unwrap(),
        )
        .await
        .unwrap();
    table
        .update::<_, Post>(
            posts::title.eq("beta").unwrap(),
            posts::nickname.set(None::<&str>).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        table
            .query()
            .filter(posts::title.eq(posts::nickname).unwrap())
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        table
            .query()
            .filter(posts::title.ne(posts::nickname).unwrap())
            .count()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        table
            .query()
            .filter(posts::counter.gte(posts::counter).unwrap())
            .count()
            .await
            .unwrap(),
        2
    );
    let p = table.alias("p").unwrap();
    let other = table.alias("other").unwrap();
    let counts = db
        .from(&p)
        .inner_join(
            &other,
            p.column(posts::title)
                .eq(other.column(posts::title))
                .unwrap(),
        )
        .unwrap()
        .filter(
            p.column(posts::counter)
                .gte(other.column(posts::counter))
                .unwrap(),
        )
        .having(count_rows().eq(p.column(posts::id).count()).unwrap())
        .select(count_rows())
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(counts, vec![2]);
    let selected = p.column(posts::title).select::<String>();
    let rows = db
        .from(&p)
        .filter(selected.eq(p.column(posts::nickname)).unwrap())
        .select(selected)
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(rows, vec!["alpha"]);
    owner.close().await;
}

#[compio::test]
async fn typed_comparison_operands_sqlite() {
    Box::pin(compare_values_and_expressions(
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
async fn typed_comparison_operands_postgres() {
    Box::pin(compare_values_and_expressions(
        CollectionFixture::postgres_native(
            "posts",
            posts::Entity::schema().clone(),
            fixtures::post_migration_fields(),
        )
        .await,
    ))
    .await;
}
