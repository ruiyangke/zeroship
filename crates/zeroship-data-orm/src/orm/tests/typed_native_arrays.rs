#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use thread-local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;

include!("../../../tests/fixtures/native_arrays_schema.rs");
native_arrays_schema!(pub native_schema);
use native_schema::grants;

#[derive(Debug, PartialEq, FromRow)]
#[orm(entity = grants)]
struct Grant {
    id: String,
    scopes: Vec<String>,
    amr: Option<Vec<String>>,
    tags: Vec<String>,
}

#[derive(Insertable)]
#[orm(entity = grants)]
struct NewGrant<'a> {
    id: &'a str,
    scopes: &'a [String],
    amr: Option<Vec<&'a str>>,
    tags: Vec<&'a str>,
}

#[derive(Changeset)]
#[orm(entity = grants)]
struct Narrow {
    scopes: Change<Vec<String>>,
    amr: Change<Option<Vec<String>>>,
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn descriptor() -> Value {
    value!({
        "id":{"type":"string", "required":true, "primaryKey":true},
        "scopes":{"type":"textArray", "required":true},
        "amr":{"type":"textArray"},
        "tags":{"type":"array", "items":"string", "required":true}
    })
}

async fn fixture(postgres: bool) -> CollectionFixture {
    let columns = if postgres {
        "id TEXT PRIMARY KEY, scopes TEXT[] NOT NULL, amr TEXT[], tags JSONB NOT NULL"
    } else {
        "id TEXT PRIMARY KEY, scopes TEXT NOT NULL, amr TEXT, tags TEXT NOT NULL"
    };
    let mut owner = if postgres {
        CollectionFixture::postgres_from_table_definition("grants", descriptor(), columns).await
    } else {
        CollectionFixture::sqlite_from_table_definition("grants", descriptor(), columns).await
    };
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        native_schema::schema(),
    )
    .unwrap();
    owner
}

#[test]
fn native_declarations_match_the_migration_descriptor() {
    let generated = native_schema::schema();
    let decoded = Schema::from_collections([("grants".to_owned(), descriptor())]).unwrap();
    assert_eq!(generated, decoded);
    let fields = <grants::Entity as Entity>::schema();
    assert_eq!(
        fields["scopes"].storage.array,
        crate::schema::ArrayStorage::Native
    );
    assert_eq!(
        fields["amr"].storage.array,
        crate::schema::ArrayStorage::Native
    );
    assert_eq!(
        fields["tags"].storage.array,
        crate::schema::ArrayStorage::Json
    );

    let json_declared = Schema::from_collections([(
        "grants".to_owned(),
        value!({
            "id":{"type":"string", "required":true, "primaryKey":true},
            "scopes":{"type":"array", "items":"string", "required":true},
            "amr":{"type":"textArray"},
            "tags":{"type":"array", "items":"string", "required":true}
        }),
    )])
    .unwrap();
    assert_ne!(generated, json_declared);
}

#[expect(clippy::too_many_lines, reason = "shared backend conformance scenario")]
async fn exercise(postgres: bool) {
    let owner = fixture(postgres).await;
    let db = &owner.database;
    let table = db.entity::<grants::Entity>().unwrap();
    let scopes = owned(&["openid", "email", "email"]);
    let inserted: Grant = table
        .insert(NewGrant {
            id: "g1",
            scopes: &scopes,
            amr: Some(vec!["pwd", "otp"]),
            tags: vec!["first"],
        })
        .await
        .unwrap();
    assert_eq!(
        inserted,
        Grant {
            id: "g1".into(),
            scopes: scopes.clone(),
            amr: Some(owned(&["pwd", "otp"])),
            tags: owned(&["first"]),
        }
    );
    let empty: Grant = table
        .insert(NewGrant {
            id: "g2",
            scopes: &[],
            amr: None,
            tags: vec![],
        })
        .await
        .unwrap();
    assert_eq!((empty.scopes, empty.amr), (vec![], None));
    let (null_text, reordered) = (owned(&["NULL"]), owned(&["email", "openid"]));
    let many: Vec<Grant> = table
        .insert_many(vec![
            NewGrant {
                id: "g3",
                scopes: &null_text,
                amr: Some(vec![]),
                tags: vec![],
            },
            NewGrant {
                id: "g4",
                scopes: &reordered,
                amr: Some(vec!["NULL"]),
                tags: vec![],
            },
        ])
        .await
        .unwrap();
    assert_eq!(many[0].scopes, owned(&["NULL"]));
    assert_eq!(many[1].amr, Some(owned(&["NULL"])));

    let ids = |rows: Vec<Grant>| rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
    let find = |filter: Filter<grants::Entity>| {
        table
            .query()
            .filter(filter)
            .order_by(grants::id.asc())
            .all::<Grant>()
    };
    assert_eq!(
        ids(find(grants::scopes.eq(&scopes[..]).unwrap()).await.unwrap()),
        ["g1"]
    );
    assert_eq!(
        ids(find(grants::scopes.eq(vec!["openid", "email"]).unwrap())
            .await
            .unwrap()),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(find(grants::scopes.eq(vec!["email", "openid"]).unwrap())
            .await
            .unwrap()),
        ["g4"]
    );
    assert_eq!(
        ids(find(grants::scopes.ne(Vec::<String>::new()).unwrap())
            .await
            .unwrap()),
        ["g1", "g3", "g4"]
    );
    assert_eq!(
        ids(
            find(grants::scopes.in_values([vec!["NULL"], vec![]]).unwrap())
                .await
                .unwrap()
        ),
        ["g2", "g3"]
    );
    assert_eq!(ids(find(grants::amr.is_null()).await.unwrap()), ["g2"]);
    assert_eq!(
        ids(find(grants::amr.eq(Some(Vec::<String>::new())).unwrap())
            .await
            .unwrap()),
        ["g3"]
    );
    assert_eq!(
        ids(find(grants::amr.eq(Some(vec!["NULL"])).unwrap())
            .await
            .unwrap()),
        ["g4"]
    );
    assert_eq!(
        table
            .count(grants::amr.not_in_values([Some(vec!["NULL"])]).unwrap())
            .await
            .unwrap(),
        2
    );

    let updated: Grant = table
        .update(
            grants::id.eq("g1").unwrap(),
            grants::scopes
                .pull("email")
                .unwrap()
                .and(grants::amr.push("otp").unwrap())
                .unwrap()
                .and(grants::tags.add_to_set("first").unwrap())
                .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.scopes, owned(&["openid"]));
    assert_eq!(updated.amr, Some(owned(&["pwd", "otp", "otp"])));
    assert_eq!(updated.tags, owned(&["first"]));
    let narrowed: Grant = table
        .update(
            grants::id.eq("g1").unwrap(),
            Narrow {
                scopes: Change::Set(owned(&["b", "a", "a"])),
                amr: Change::Set(None),
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(narrowed.scopes, owned(&["b", "a", "a"]));
    assert_eq!(narrowed.amr, None);
    let unchanged: Grant = table
        .update(
            grants::id.eq("g1").unwrap(),
            grants::amr.add_to_set("pwd").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.amr, None);

    let replacement = owned(&["z"]);
    let upserted: Grant = table
        .upsert(
            NewGrant {
                id: "g1",
                scopes: &replacement,
                amr: Some(vec!["sso"]),
                tags: vec!["t"],
            },
            ConflictTarget::new(grants::id),
        )
        .await
        .unwrap();
    assert_eq!(upserted.scopes, owned(&["z"]));
    assert_eq!(upserted.amr, Some(owned(&["sso"])));

    let g = table.alias("g").unwrap();
    let selected: Vec<(String, Vec<String>, Option<Vec<String>>)> = db
        .from(&g)
        .filter(
            g.column(grants::scopes)
                .eq(vec!["email", "openid"])
                .unwrap(),
        )
        .select((
            g.column(grants::id).select::<String>(),
            g.column(grants::scopes).select::<Vec<String>>(),
            g.column(grants::amr).select::<Option<Vec<String>>>(),
        ))
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(
        selected,
        [(
            "g4".to_owned(),
            owned(&["email", "openid"]),
            Some(owned(&["NULL"]))
        )]
    );
    let members: Vec<String> = db
        .from(&g)
        .filter(
            g.column(grants::scopes)
                .in_values([vec!["NULL"], vec!["z"]])
                .unwrap()
                .and(g.column(grants::amr).ne(Some(vec!["other"])).unwrap()),
        )
        .order_by(g.column(grants::id).asc())
        .select(g.column(grants::id).select::<String>())
        .unwrap()
        .all()
        .await
        .unwrap();
    assert_eq!(members, ["g1", "g3"]);

    let invalid_scopes = owned(&["private_element\u{0}"]);
    for error in [
        table
            .insert::<_, Grant>(NewGrant {
                id: "bad",
                scopes: &invalid_scopes,
                amr: None,
                tags: vec![],
            })
            .await
            .unwrap_err(),
        table
            .update::<_, Grant>(
                grants::id.eq("g1").unwrap(),
                grants::scopes.push("private_element\u{0}").unwrap(),
            )
            .await
            .unwrap_err(),
    ] {
        assert_eq!(error.code(), "invalid_array_element", "{error:?}");
        assert!(!error.message_str().contains("private_element"));
    }
    let tags = table.alias("t").unwrap();
    macro_rules! refused {
        ($builder:expr) => {
            match $builder {
                Err(_) => true,
                Ok(builder) => builder.all().await.is_err(),
            }
        };
    }
    assert!(refused!(db
        .from(&tags)
        .group_by(tags.column(grants::scopes))
        .select(count_rows())));
    assert!(refused!(db
        .from(&tags)
        .order_by(tags.column(grants::scopes).asc())
        .select(tags.column(grants::id).select::<String>())));
    assert!(refused!(db
        .from(&tags)
        .select(tags.column(grants::scopes).count_distinct())));
    assert!(!refused!(db
        .from(&tags)
        .order_by(tags.column(grants::id).asc())
        .select(tags.column(grants::scopes).select::<Vec<String>>())));
    assert_eq!(
        db.from(&tags)
            .select(tags.column(grants::scopes).count())
            .unwrap()
            .all()
            .await
            .unwrap(),
        vec![4]
    );

    let before = table
        .query()
        .order_by(grants::id.asc())
        .all::<Grant>()
        .await
        .unwrap();
    let result: Result<(), DbError> = db
        .transaction(|tx| async move {
            let grants_in_tx = tx.entity::<grants::Entity>()?;
            grants_in_tx
                .update_many(Filter::all(), grants::scopes.push("rolled_back")?)
                .await?;
            Err(DbError::validation("rollback_test", "abort"))
        })
        .await;
    assert!(result.is_err());
    assert_eq!(
        table
            .query()
            .order_by(grants::id.asc())
            .all::<Grant>()
            .await
            .unwrap(),
        before
    );
    owner.close().await;
}

#[compio::test]
async fn typed_native_arrays_postgres() {
    Box::pin(exercise(true)).await;
}

#[compio::test]
async fn typed_native_arrays_sqlite() {
    Box::pin(exercise(false)).await;
}
