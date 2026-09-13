use super::fixtures::CollectionFixture;
use super::*;

posts_schema!(pub read_schema, readings);
use read_schema::readings;

#[derive(Debug, FromRow)]
#[orm(entity = readings)]
struct Reading {
    title: String,
    counter: i64,
}

#[derive(Insertable)]
#[orm(entity = readings)]
struct NewReading<'a> {
    title: &'a str,
    counter: i64,
    score: Option<f64>,
    nickname: Option<&'a str>,
}

#[compio::test]
async fn typed_reads_keep_execution_state_off_the_callers_stack() {
    const CALLER_FUTURE_BUDGET: usize = 2048;
    let owner = CollectionFixture::sqlite_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    let db = &owner.database;
    let entity = db.entity::<readings::Entity>().unwrap();
    let _: Reading = entity
        .insert(NewReading {
            title: "measured",
            counter: 1,
            score: None,
            nickname: None,
        })
        .await
        .unwrap();
    let alias = entity.alias("r").unwrap();
    let find = entity.find::<Reading>(Filter::all(), FindOptions::default());
    let all = entity.query().all::<Reading>();
    let first = entity.query().first::<Reading>();
    let count = entity.count(Filter::all());
    let exists = entity.exists(Filter::all());
    let projected = db
        .from(&alias)
        .select(alias.row::<Reading>())
        .unwrap()
        .all();
    let dynamic = entity.collection.find(value!({}), value!({}));
    let caller = async {
        let found = entity
            .find::<Reading>(Filter::all(), FindOptions::default())
            .await?;
        let queried = entity.query().all::<Reading>().await?;
        Ok::<_, DbError>((found, queried))
    };
    let sizes = [
        ("find", std::mem::size_of_val(&find)),
        ("query.all", std::mem::size_of_val(&all)),
        ("query.first", std::mem::size_of_val(&first)),
        ("count", std::mem::size_of_val(&count)),
        ("exists", std::mem::size_of_val(&exists)),
        ("projection.all", std::mem::size_of_val(&projected)),
        ("caller", std::mem::size_of_val(&caller)),
    ];
    eprintln!(
        "typed read future sizes: {sizes:?}; dynamic baseline: {}",
        std::mem::size_of_val(&dynamic)
    );
    assert_eq!(find.await.unwrap().len(), 1);
    assert_eq!(all.await.unwrap().len(), 1);
    assert_eq!(first.await.unwrap().unwrap().title, "measured");
    assert_eq!(count.await.unwrap(), 1);
    assert!(exists.await.unwrap());
    assert_eq!(projected.await.unwrap().len(), 1);
    assert!(matches!(dynamic.await.unwrap(), Output::Rows {rows, ..} if rows.len() == 1));
    let (found, queried) = caller.await.unwrap();
    assert_eq!((found.len(), queried.len()), (1, 1));
    owner.close().await;
    for (operation, size) in sizes {
        assert!(
            size <= CALLER_FUTURE_BUDGET,
            "{operation} future exceeds the caller stack budget: {size}"
        );
    }
}

async fn exercise(db: &Database) {
    let readings = db.entity::<readings::Entity>().unwrap();
    for (title, counter, score, nickname) in [
        ("alpha", 10, Some(1.0), Some("one")),
        ("beta", 20, None, Some("one")),
        ("gamma", 30, Some(3.0), Some("two")),
    ] {
        let _: Reading = readings
            .insert(NewReading {
                title,
                counter,
                score,
                nickname,
            })
            .await
            .unwrap();
    }
    let page: Vec<Reading> = readings
        .query()
        .filter(Filter::all())
        .order_by(readings::counter.desc())
        .offset(1)
        .unwrap()
        .limit(1)
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!((&*page[0].title, page[0].counter), ("beta", 20));
    assert_eq!(readings.count(Filter::all()).await.unwrap(), 3);
    assert_eq!(
        readings
            .query()
            .offset(1)
            .unwrap()
            .limit(1)
            .unwrap()
            .count()
            .await
            .unwrap(),
        3
    );
    let first: Reading = readings
        .query()
        .order_by(readings::counter.asc())
        .first()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.title, "alpha");

    let r = readings.alias("r").unwrap();
    type GroupSummary = (
        Option<String>,
        i64,
        Option<i64>,
        Option<f64>,
        Option<i64>,
        Option<i64>,
    );
    let grouped: Vec<GroupSummary> = db
        .from(&r)
        .group_by(r.column(readings::nickname))
        .having(count_rows().gt(1_i64).unwrap())
        .select((
            r.column(readings::nickname).select::<Option<String>>(),
            count_rows(),
            r.column(readings::counter).sum(),
            r.column(readings::counter).avg(),
            r.column(readings::counter).min::<i64>(),
            r.column(readings::counter).max::<i64>(),
        ))
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(
        grouped,
        vec![(
            Some("one".into()),
            2,
            Some(30),
            Some(15.0),
            Some(10),
            Some(20)
        )]
    );

    let totals = db
        .from(&r)
        .select((
            count_rows(),
            r.column(readings::score).count(),
            r.column(readings::nickname).count_distinct(),
            r.column(readings::score).avg(),
        ))
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(totals, vec![(3, 2, 2, Some(2.0))]);
    let average = db
        .from(&r)
        .having(r.column(readings::counter).avg().gt(19.5).unwrap())
        .select(r.column(readings::counter).avg())
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(average, vec![Some(20.0)]);
    let text_count = db
        .from(&r)
        .having(r.column(readings::nickname).count().gt(2_i64).unwrap())
        .select(r.column(readings::nickname).count())
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(text_count, vec![3]);
    let empty = db
        .from(&r)
        .filter(r.column(readings::title).eq("absent").unwrap())
        .select((
            count_rows(),
            r.column(readings::counter).sum(),
            r.column(readings::score).avg(),
            r.column(readings::counter).min::<i64>(),
        ))
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(empty, vec![(0, None, None, None)]);

    let scalar: Vec<(String, Option<f64>)> = db
        .from(&r)
        .select((
            r.column(readings::title).select(),
            r.column(readings::score).select(),
        ))
        .unwrap()
        .order_by(r.column(readings::counter).asc())
        .offset(1)
        .unwrap()
        .limit(1)
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(scalar, vec![("beta".into(), None)]);

    let child = readings.alias("child").unwrap();
    let left: Vec<(String, Option<String>)> = db
        .from(&r)
        .left_join(
            &child,
            r.column(readings::nickname)
                .eq_column(child.column(readings::title))
                .unwrap(),
        )
        .unwrap()
        .filter(r.column(readings::title).eq("alpha").unwrap())
        .select((
            r.column(readings::title).select(),
            child.column(readings::title).select_optional(),
        ))
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(left, vec![("alpha".into(), None)]);

    db.collection("readings")
        .unwrap()
        .delete(value!({"title":"gamma"}))
        .await
        .unwrap();
    assert_eq!(readings.count(Filter::all()).await.unwrap(), 2);
    assert_eq!(readings.query().include_deleted().count().await.unwrap(), 3);

    let escaped = db
        .transaction(|tx| async move {
            let readings = tx.entity::<readings::Entity>()?;
            let _: Reading = readings
                .insert(NewReading {
                    title: "tx",
                    counter: 40,
                    score: None,
                    nickname: None,
                })
                .await?;
            assert_eq!(readings.count(Filter::all()).await?, 3);
            Ok(readings.query())
        })
        .await
        .unwrap();
    assert!(matches!(
        escaped.all::<Reading>().await,
        Err(DbError::ValidationFailed {
            code: "transaction_scope_expired",
            ..
        })
    ));
}

#[compio::test]
async fn sqlite_typed_reads_cover_paging_grouping_and_native_scalars() {
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
async fn postgres_typed_reads_cover_paging_grouping_and_native_scalars() {
    let owner = CollectionFixture::postgres_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    exercise(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn typed_read_aliases_refuse_replaced_generated_metadata() {
    let owner = CollectionFixture::sqlite_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    let db = &owner.database;
    let entity = db.entity::<readings::Entity>().unwrap();
    let alias = entity.alias("r").unwrap();
    let built = db.from(&alias).select(alias.row::<Reading>()).unwrap();
    let scalar = db
        .from(&alias)
        .select(alias.column(readings::title).select::<String>())
        .unwrap();
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
    for result in [
        built.all().await.map(|_| ()),
        scalar.all().await.map(|_| ()),
    ] {
        assert!(
            matches!(
                result,
                Err(DbError::Configuration {
                    code: "orm_schema_mismatch",
                    ..
                })
            ),
            "{result:?}"
        );
    }
    assert!(db.from(&alias).select(alias.row::<Reading>()).is_err());
    owner.close().await;
}

#[compio::test]
async fn typed_selections_reject_foreign_aliases_with_matching_names() {
    let first = CollectionFixture::sqlite_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    let second = CollectionFixture::sqlite_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    let a = first
        .database
        .entity::<readings::Entity>()
        .unwrap()
        .alias("r")
        .unwrap();
    let b = second
        .database
        .entity::<readings::Entity>()
        .unwrap()
        .alias("r")
        .unwrap();
    assert!(first.database.from(&a).select(b.row::<Reading>()).is_err());
    assert!(first
        .database
        .from(&a)
        .select(b.column(readings::title).select::<String>())
        .is_err());
    assert!(first
        .database
        .from(&a)
        .select(b.column(readings::counter).sum())
        .is_err());
    assert!(first
        .database
        .from(&a)
        .group_by(b.column(readings::title))
        .select(count_rows())
        .is_err());
    first.close().await;
    second.close().await;
}

#[compio::test]
async fn typed_sources_and_projections_keep_their_originating_transaction_scope() {
    let owner = CollectionFixture::sqlite_native(
        "readings",
        readings::Entity::schema().clone(),
        super::fixtures::post_migration_fields(),
    )
    .await;
    let db = &owner.database;
    let outside = db.clone();
    let (alias, row, scalar, built, prepared, grouped) = db
        .transaction(|tx| async move {
            let alias = tx.entity::<readings::Entity>()?.alias("r")?;
            let live = outside.entity::<readings::Entity>()?.alias("r")?;
            let built = outside.from(&alias).select(alias.row::<Reading>())?;
            let prepared = outside.from(&alias).select(alias.row::<Reading>())?.all();
            let grouped = outside
                .from(&live)
                .group_by(alias.column(readings::title))
                .select(count_rows())?;
            let row = alias.row::<Reading>();
            let scalar = alias.column(readings::title).select::<String>();
            Ok((alias, row, scalar, built, prepared, grouped))
        })
        .await
        .unwrap();
    let live = db.entity::<readings::Entity>().unwrap().alias("r").unwrap();
    for result in [
        db.from(&alias).select(live.row::<Reading>()).map(|_| ()),
        db.from(&live).select(row).map(|_| ()),
        db.from(&live).select(scalar).map(|_| ()),
        built.all().await.map(|_| ()),
        prepared.await.map(|_| ()),
        grouped.all().await.map(|_| ()),
    ] {
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
    owner.close().await;
}
