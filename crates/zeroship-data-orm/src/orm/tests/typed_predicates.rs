use super::fixtures::CollectionFixture;
use super::*;

include!("../../../tests/fixtures/predicates_schema.rs");
predicates_schema!(pub predicate_schema);
use predicate_schema::predicate_rows as rows;

#[derive(Debug, FromRow)]
#[orm(entity = rows)]
struct Label {
    label: String,
}

#[compio::test]
async fn sqlite_typed_predicates_preserve_native_values_and_null_semantics() {
    let mut owner = CollectionFixture::sqlite_native(
        "predicate_rows",
        rows::Entity::schema().clone(),
        super::fixtures::predicate_migration_fields(),
    )
    .await;
    owner
        .replace_from_migration(
            "predicate_rows",
            include_str!("../../../tests/fixtures/typed-predicates-migration.json"),
        )
        .await;
    exercise(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_typed_predicates_preserve_native_values_and_null_semantics() {
    let mut owner = CollectionFixture::postgres_native(
        "predicate_rows",
        rows::Entity::schema().clone(),
        super::fixtures::predicate_migration_fields(),
    )
    .await;
    owner
        .replace_from_migration(
            "predicate_rows",
            include_str!("../../../tests/fixtures/typed-predicates-migration.json"),
        )
        .await;
    exercise(&owner.database).await;
    owner.close().await;
}

struct Secret;
impl Column for Secret {
    type Entity = rows::Entity;
    type SqlType = sql_types::Nullable<sql_types::Text>;
    const NAME: &'static str = "secret";
}
impl FilterableColumn for Secret {}

#[test]
fn predicate_fixture_matches_the_migration_artifact() {
    let migration: zeroship_migrate::model::ir::MigrationIr = serde_json::from_str(include_str!(
        "../../../tests/fixtures/typed-predicates-migration.json"
    ))
    .unwrap();
    let policy = zeroship_migrate::effective_policy_from_charter_toml(
        zeroship_migrate_server::policy::CONFINED_CEILING_TOML,
    )
    .unwrap();
    let artifacts = zeroship_migrate::render_artifacts(
        zeroship_migrate::shipping_vendors(),
        &migration.ops,
        &zeroship_migrate_postgres::DIALECT,
        "predicate_fixture",
        &policy,
    )
    .unwrap();
    assert_eq!(
        artifacts.runtime_json,
        include_str!("../../../tests/fixtures/typed-predicates.runtime.json")
    );
    let decoded =
        CollectionSchema::from_fields(&super::fixtures::predicate_migration_fields()).unwrap();
    assert_eq!(rows::Entity::schema(), &decoded);
}

async fn exercise(db: &Database) {
    let collection = db.collection("predicate_rows").unwrap();
    for (label, rank, optional, payload, document) in [
        ("a", 1_i64, None, Some(vec![0_u8, 255]), value!({"$eq":1})),
        ("b", 2, Some("one"), None, value!({"$eq":2})),
        ("c", 3, Some("two"), Some(vec![1, 254]), value!({"$eq":3})),
    ] {
        let mut record =
            value!({"label":label, "rank":rank, "optional":optional, "document":document});
        record.as_object_mut().unwrap().insert(
            "payload".into(),
            payload.map(Value::Bytes).unwrap_or(Value::Null),
        );
        record
            .as_object_mut()
            .unwrap()
            .insert("moment".into(), Value::Timestamp(rank * 1000));
        collection.insert(record).await.unwrap();
    }
    let entity = db.entity::<rows::Entity>().unwrap();
    let conjunction = (0..=crate::sql::MAX_PREDICATE_DEPTH).fold(Filter::all(), |filter, _| {
        filter.and(rows::rank.gte(1).unwrap())
    });
    let disjunction = (0..=crate::sql::MAX_PREDICATE_DEPTH).fold(!Filter::all(), |filter, _| {
        filter.or(rows::rank.eq(2).unwrap())
    });
    assert_labels(
        entity
            .find(conjunction, FindOptions::default())
            .await
            .unwrap(),
        vec!["a", "b", "c"],
    );
    assert_labels(
        entity
            .find(disjunction, FindOptions::default())
            .await
            .unwrap(),
        vec!["b"],
    );
    for (filter, expected) in [
        (rows::rank.ne(2).unwrap(), vec!["a", "c"]),
        (rows::rank.lt(2).unwrap(), vec!["a"]),
        (rows::rank.lte(2).unwrap(), vec!["a", "b"]),
        (rows::rank.gt(2).unwrap(), vec!["c"]),
        (rows::rank.gte(2).unwrap(), vec!["b", "c"]),
        (rows::rank.in_values([1, 3]).unwrap(), vec!["a", "c"]),
        (rows::rank.not_in_values([1, 3]).unwrap(), vec!["b"]),
        (rows::optional.is_null(), vec!["a"]),
        (rows::optional.is_not_null(), vec!["b", "c"]),
        (rows::optional.ne(None::<&str>).unwrap(), vec!["b", "c"]),
        (
            rows::optional.in_values([None, Some("one")]).unwrap(),
            vec!["a", "b"],
        ),
        (
            rows::optional.not_in_values([None, Some("one")]).unwrap(),
            vec!["c"],
        ),
        (rows::optional.in_values([None::<&str>]).unwrap(), vec!["a"]),
        (
            rows::optional.not_in_values([None::<&str>]).unwrap(),
            vec!["b", "c"],
        ),
        (rows::rank.in_values([] as [i64; 0]).unwrap(), vec![]),
        (
            rows::rank.not_in_values([] as [i64; 0]).unwrap(),
            vec!["a", "b", "c"],
        ),
        (
            rows::payload.in_values([Some(vec![0_u8, 255])]).unwrap(),
            vec!["a"],
        ),
        (
            rows::document
                .in_values([value!(3), value!({"$eq":2})])
                .unwrap(),
            vec!["b"],
        ),
        (rows::moment.gte(2000_i64).unwrap(), vec!["b", "c"]),
        (!rows::rank.eq(2).unwrap(), vec!["a", "c"]),
        (
            rows::rank
                .lt(2)
                .unwrap()
                .or(rows::rank.gt(2).unwrap())
                .negate(),
            vec!["b"],
        ),
    ] {
        let found: Vec<Label> = entity.find(filter, FindOptions::default()).await.unwrap();
        assert_labels(found, expected);
    }
    let source = entity.alias("p").unwrap();
    for (filter, expected) in [
        (source.column(rows::rank).ne(2).unwrap(), vec!["a", "c"]),
        (source.column(rows::rank).lt(2).unwrap(), vec!["a"]),
        (source.column(rows::rank).lte(2).unwrap(), vec!["a", "b"]),
        (source.column(rows::rank).gt(2).unwrap(), vec!["c"]),
        (source.column(rows::rank).gte(2).unwrap(), vec!["b", "c"]),
        (
            source.column(rows::rank).in_values([1, 3]).unwrap(),
            vec!["a", "c"],
        ),
        (
            source.column(rows::rank).not_in_values([1, 3]).unwrap(),
            vec!["b"],
        ),
        (source.column(rows::optional).is_null(), vec!["a"]),
        (source.column(rows::optional).is_not_null(), vec!["b", "c"]),
        (
            source.column(rows::optional).ne(None::<&str>).unwrap(),
            vec!["b", "c"],
        ),
        (
            source
                .column(rows::optional)
                .in_values([None, Some("one")])
                .unwrap(),
            vec!["a", "b"],
        ),
        (
            source
                .column(rows::optional)
                .not_in_values([None, Some("one")])
                .unwrap(),
            vec!["c"],
        ),
        (
            source.column(rows::rank).in_values([] as [i64; 0]).unwrap(),
            vec![],
        ),
        (
            source
                .column(rows::rank)
                .not_in_values([] as [i64; 0])
                .unwrap(),
            vec!["a", "b", "c"],
        ),
        (
            source
                .column(rows::payload)
                .in_values([Some(vec![0_u8, 255])])
                .unwrap(),
            vec!["a"],
        ),
        (
            source
                .column(rows::document)
                .in_values([value!(3), value!({"$eq":2})])
                .unwrap(),
            vec!["b"],
        ),
        (
            source.column(rows::moment).gte(2000_i64).unwrap(),
            vec!["b", "c"],
        ),
        (
            source.column(rows::rank).eq(2).unwrap().negate(),
            vec!["a", "c"],
        ),
    ] {
        let found = db
            .from(&source)
            .filter(filter)
            .select(source.row::<Label>())
            .unwrap()
            .all()
            .await
            .unwrap();
        assert_labels(found, expected);
    }
    assert!(entity
        .find::<Label>(
            rows::optional.lt(None::<&str>).unwrap(),
            FindOptions::default()
        )
        .await
        .is_err());
    assert!(source.column(rows::optional).lt(None::<&str>).is_err());
    for filter in [
        source
            .column(Field::<Secret>::new())
            .in_values([] as [Option<&str>; 0])
            .unwrap(),
        source
            .column(Field::<Secret>::new())
            .not_in_values([] as [Option<&str>; 0])
            .unwrap(),
        source.column(Field::<Secret>::new()).is_null(),
    ] {
        assert!(db
            .from(&source)
            .filter(filter)
            .select(source.row::<Label>())
            .unwrap()
            .all()
            .await
            .is_err());
    }
    let too_many = crate::sql::MAX_MEMBERSHIP_LIST_LEN + 1;
    assert!(rows::rank
        .in_values(std::iter::repeat_n(1_i64, too_many))
        .is_err());
    assert!(source
        .column(rows::optional)
        .in_values(std::iter::repeat_n(None::<&str>, too_many))
        .is_err());
    let changed: Option<Label> = entity
        .update(
            rows::rank
                .in_values([1, 3])
                .unwrap()
                .and(!rows::optional.is_null()),
            rows::label.set("changed").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changed.unwrap().label, "changed");
    assert!(entity
        .delete::<Label>(rows::label.in_values(["changed"]).unwrap())
        .await
        .unwrap()
        .is_some());
}

fn assert_labels(found: Vec<Label>, expected: Vec<&str>) {
    let mut actual: Vec<_> = found.into_iter().map(|row| row.label).collect();
    actual.sort();
    assert_eq!(actual, expected);
}
