use super::*;
use crate::value;
use zeroship_data_orm::encryption::ProjectKeySource;

schema!(pub test_schema = "../../tests/fixtures/schema.runtime.json");
use test_schema::posts;

mod bulk;
mod calendar_date;
mod dynamic_reads;
mod encrypted_upsert;
mod fixtures;
mod generated_identity;
mod identity;
mod identity_contract;
mod identity_visibility;
mod joins;
mod json;
mod lifecycle;
mod nested_temporal;
mod protected_projections;
mod protected_updates;
mod schema_updates;
mod sql_registration;
mod timestamp;
mod upsert_contract;
mod typed_arrays;
mod typed_updates;
mod update_operators;
mod update_validation;

#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct Post {
    id: String,
    title: String,
    version: i64,
}
#[derive(Insertable)]
#[orm(entity = posts)]
struct NewPost {
    title: String,
}

#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct Details {
    id: String,
    created_at: i64,
    title: String,
    payload: Option<Vec<u8>>,
    counter: i64,
    nickname: Option<String>,
    score: Option<f64>,
}

#[derive(Insertable)]
#[orm(entity = posts)]
struct NewDetails {
    title: String,
    payload: Option<Vec<u8>>,
    #[orm(default)]
    counter: Defaulted<i64>,
    #[orm(default)]
    nickname: Defaulted<Option<String>>,
    score: Option<f64>,
}

#[derive(Default, Changeset)]
#[orm(entity = posts)]
struct EditDetails {
    nickname: Change<Option<String>>,
    counter: Change<i64>,
    payload: Change<Option<Vec<u8>>>,
}

#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct Summary {
    #[orm(column = "title")]
    name: String,
}

#[test]
fn metadata_fixture_is_the_migration_engines_output() {
    let migration: zeroship_migrate::model::ir::MigrationIr =
        serde_json::from_str(include_str!("../../tests/fixtures/orm-migration.json")).unwrap();
    let policy = zeroship_migrate::effective_policy_from_charter_toml(
        zeroship_migrate_server::policy::CONFINED_CEILING_TOML,
    )
    .unwrap();
    let generated = zeroship_migrate::render_artifacts(
        zeroship_migrate::shipping_vendors(),
        &migration.ops,
        &zeroship_migrate_postgres::DIALECT,
        "orm_fixture",
        &policy,
    )
    .unwrap();
    assert_eq!(
        generated.runtime_json,
        include_str!("../../tests/fixtures/schema.runtime.json")
    );
}

#[compio::test]
async fn defaults_null_changes_projections_and_native_buffers() {
    let (db, _directory) = database().await;
    exercise_registered_backend(db.clone()).await;
}

#[compio::test]
async fn sqlite_search_values_round_trip_through_the_rust_orm() {
    let (db, directory) = database().await;
    let fields = value!({
        "embedding": {"type":"vector", "vectorDims":2},
        "location": {"type":"geoPoint"},
    });
    let policy = zeroship_migrate::effective_policy_from_charter_toml(
        zeroship_migrate_server::policy::CONFINED_CEILING_TOML,
    )
    .unwrap();
    let sql = zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect(
        zeroship_migrate::shipping_vendors(),
        "main",
        "places",
        &serde_json::to_value(&fields).unwrap(),
        &zeroship_migrate::schema::query::FkEmission::Inline,
        &zeroship_migrate_sqlite::DIALECT,
        &policy,
    )
    .unwrap();
    let fixture = rusqlite::Connection::open(
        directory
            .path()
            .join(format!("zs-{}.sqlite", db.binding.app_id())),
    )
    .unwrap();
    fixture.execute_batch(&sql).unwrap();
    drop(fixture);
    let db = Database::from_schema(
        db.binding.clone(),
        db.backend.clone(),
        vec![(
            "places".into(),
            crate::tests::fixtures::schema::generated_fields(fields),
        )],
    )
    .unwrap();
    let expected = value!({"embedding":[1.0, -0.5], "location":{"lat":37.0, "lng":-122.0}});
    let document = expected.clone();
    let id = db
        .transaction(|tx| async move {
            let Output::Rows { rows, .. } =
                tx.collection("places")?.insert(document.clone()).await?
            else {
                panic!("insert must return a row")
            };
            assert_eq!(rows[0]["embedding"], document["embedding"]);
            assert_eq!(rows[0]["location"], document["location"]);
            Ok(rows[0]["id"].clone())
        })
        .await
        .unwrap();
    let Output::Rows { rows, .. } = db
        .collection("places")
        .unwrap()
        .find(value!({"id":id}), value!({}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(rows.len(), 1);
    let embedding =
        <Vec<f32> as DecodeValue<sql_types::Vector>>::decode_value(rows[0]["embedding"].clone())
            .unwrap();
    let point =
        <Point as DecodeValue<sql_types::GeoPoint>>::decode_value(rows[0]["location"].clone())
            .unwrap();
    assert_eq!(embedding, vec![1.0, -0.5]);
    assert_eq!(
        point,
        Point {
            lat: 37.0,
            lng: -122.0
        }
    );
}

#[compio::test]
async fn postgres_native_models_round_trip() {
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
    crate::tests::fixtures::reset_engine();
    let backend = Rc::new(
        zeroship_data_orm::backend::postgres::PostgresBackend::connect(
            &postgres.url(),
            4,
            ProjectKeySource::unavailable(),
        )
        .await
        .unwrap(),
    );
    let app = format!("zsorm_{}", uuid::Uuid::new_v4().simple());
    let binding = DbBinding::cold_start(&app);
    let quoted_schema = crate::sql::compile::quote_ident(&app);
    backend
        .execute_fixture(&format!("CREATE SCHEMA {quoted_schema}"), &[])
        .await
        .unwrap();
    for sql in table_statements(&app, &zeroship_migrate_postgres::DIALECT) {
        backend.execute_fixture(&sql, &[]).await.unwrap();
    }
    crate::tests::fixtures::roles::ensure_per_app_role(backend.pool(), &app)
        .await
        .unwrap();
    let role = zeroship_core::database_role::per_app_role_name(&app).unwrap();
    let quoted_role = crate::sql::compile::quote_ident(&role);
    backend
        .execute_fixture(
            &format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON {quoted_schema}.posts TO {quoted_role}"
            ),
            &[],
        )
        .await
        .unwrap();
    let db = Database::connect(
        binding,
        crate::ConnectOptions::new(postgres.url(), ProjectKeySource::unavailable()),
        vec![("posts".into(), <posts::Entity as Entity>::schema().clone())],
    )
    .await
    .unwrap();
    exercise_registered_backend(db.clone()).await;
    drop(db);
    backend
        .execute_fixture(&format!("DROP SCHEMA {quoted_schema} CASCADE"), &[])
        .await
        .unwrap();
    backend
        .execute_fixture(&format!("DROP OWNED BY {quoted_role}"), &[])
        .await
        .unwrap();
    backend
        .execute_fixture(&format!("DROP ROLE {quoted_role}"), &[])
        .await
        .unwrap();
}

async fn exercise_native_models(db: &Database) {
    let records = db.entity::<posts::Entity>().unwrap();
    let row: Details = records
        .insert(NewDetails {
            title: "native".into(),
            payload: Some(vec![0, 255, 128]),
            counter: Defaulted::Default,
            nickname: Defaulted::Default,
            score: Some(1.25),
        })
        .await
        .unwrap();
    assert_eq!(row.title, "native");
    assert_eq!(row.payload, Some(vec![0, 255, 128]));
    assert!(row.created_at > 0);
    assert_eq!(row.counter, 7);
    assert_eq!(row.nickname.as_deref(), Some("anonymous"));
    assert_eq!(row.score, Some(1.25));

    let edited: Details = records
        .update(
            posts::id.eq(row.id.clone()).unwrap(),
            EditDetails {
                nickname: Change::Set(None),
                counter: Change::Set(i64::MAX),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.nickname, None);
    assert_eq!(edited.counter, i64::MAX);
    assert_eq!(edited.payload, row.payload);

    let null: Details = records
        .insert(NewDetails {
            title: "explicit null".into(),
            payload: None,
            counter: Defaulted::Value(9),
            nickname: Defaulted::Value(None),
            score: None,
        })
        .await
        .unwrap();
    assert_eq!(null.nickname, None);
    assert_eq!(null.counter, 9);

    let summary: Vec<Summary> = records
        .find(posts::title.eq("native").unwrap(), Default::default())
        .await
        .unwrap();
    assert_eq!(summary[0].name, "native");
    assert_eq!(Summary::COLUMNS, &["title"]);

    let failed: Result<Details, _> = records
        .insert(NewDetails {
            title: "invalid".into(),
            payload: None,
            counter: Defaulted::Default,
            nickname: Defaulted::Default,
            score: Some(f64::NAN),
        })
        .await;
    assert!(failed.unwrap_err().to_string().contains("posts.score"));
    let absent: Vec<Summary> = records
        .find(posts::title.eq("invalid").unwrap(), Default::default())
        .await
        .unwrap();
    assert!(absent.is_empty());

    let result: Result<(), DbError> = db
        .transaction(|tx| async move {
            let records = tx.entity::<posts::Entity>()?;
            let _: Post = records
                .insert(NewPost {
                    title: "rollback derived".into(),
                })
                .await?;
            Err(DbError::internal("rollback"))
        })
        .await;
    assert!(result.is_err());
    let rows: Vec<Summary> = records
        .find(
            posts::title.eq("rollback derived").unwrap(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[derive(Debug, PartialEq, Eq)]
struct Title(String);
impl EncodeValue<sql_types::Text> for Title {
    fn encode_value(self) -> Result<Value, DbError> {
        if self.0.is_empty() {
            Err(DbError::validation(
                "empty_title",
                "title must not be empty",
            ))
        } else {
            Ok(Value::String(self.0))
        }
    }
}
impl DecodeValue<sql_types::Text> for Title {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        <String as DecodeValue<sql_types::Text>>::decode_value(value).map(Self)
    }
}
#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct NativeProjection {
    title: Title,
    payload: Option<Vec<u8>>,
}
#[derive(Insertable)]
#[orm(entity = posts)]
struct DomainInsert {
    title: Title,
    payload: Option<Vec<u8>>,
}

#[test]
fn derives_move_allocations_and_contextualize_codec_errors() {
    let text = String::from("domain title");
    let text_address = text.as_ptr();
    let bytes = vec![0, 255, 128];
    let bytes_address = bytes.as_ptr();
    let record = DomainInsert {
        title: Title(text),
        payload: Some(bytes),
    }
    .into_record()
    .unwrap();
    assert_eq!(record["title"].as_str().unwrap().as_ptr(), text_address);
    assert_eq!(
        record["payload"].as_bytes().unwrap().as_ptr(),
        bytes_address
    );
    let projection = NativeProjection::from_row(Row::new(record)).unwrap();
    assert_eq!(projection.title.0.as_ptr(), text_address);
    let decoded_bytes = projection.payload.unwrap();
    assert_eq!(decoded_bytes.as_ptr(), bytes_address);

    let error = DomainInsert {
        title: Title(String::new()),
        payload: None,
    }
    .into_record()
    .unwrap_err();
    assert!(error.message_str().contains("posts.title: encode"));
    let error = NativeProjection::from_row(Row::new(Record::new())).unwrap_err();
    assert!(error.message_str().contains("posts.title: missing field"));
    let error = NativeProjection::from_row(Row::new([("title".into(), Value::Bool(true))].into()))
        .unwrap_err();
    assert!(error.message_str().contains("posts.title: decode"));
}

#[test]
fn native_codecs_check_ranges_and_protected_values() {
    use sql_types::*;
    assert!(<u64 as EncodeValue<BigInt>>::encode_value(u64::MAX).is_err());
    assert!(<i64 as EncodeValue<Integer>>::encode_value(i64::MAX).is_err());
    assert!(<u8 as DecodeValue<Integer>>::decode_value(Value::from(-1)).is_err());
    assert!(<f64 as EncodeValue<Number>>::encode_value(f64::INFINITY).is_err());
    assert!(
        <f32 as DecodeValue<Number>>::decode_value(Value::try_from(f64::MAX).unwrap()).is_err()
    );
    let exact = Decimal("12345678901234567890.123456789".into());
    let encoded = <Decimal as EncodeValue<Number>>::encode_value(exact.clone()).unwrap();
    assert_eq!(
        <Decimal as DecodeValue<Number>>::decode_value(encoded).unwrap(),
        exact
    );
    assert!(<Decimal as EncodeValue<Number>>::encode_value(Decimal("true".into())).is_err());
    let mut sentinel = value!({
        "sentinel": "__zsmask__", "masked": "***", "classification": "pii",
        "_sig": "forged"
    });
    assert!(<Protected<String> as DecodeValue<Text>>::decode_value(sentinel.clone()).is_err());
    sentinel["_sig"] = Value::from(crate::protection::mask_pass::mask_sentinel_signature());
    assert_eq!(
        <Protected<String> as DecodeValue<Text>>::decode_value(sentinel).unwrap(),
        Protected::Masked {
            display: "***".into(),
            classification: "pii".into()
        }
    );
    assert_eq!(
        <Option<Protected<String>> as DecodeValue<Nullable<Text>>>::decode_value(Value::Null)
            .unwrap(),
        None
    );
}

#[compio::test]
async fn typed_handles_refuse_descriptor_drift() {
    let (db, _directory) = database().await;
    let records = db.entity::<posts::Entity>().unwrap();
    let mut changed = <posts::Entity as Entity>::schema().clone();
    changed["title"]["type"] = Value::from("boolean");
    db.context().with(|| {
        zeroship_data_orm::schema_cache::with_mut(|cache| {
            cache.insert_one(db.binding(), "posts", changed);
        })
    });
    assert!(matches!(
        db.entity::<posts::Entity>(),
        Err(DbError::Configuration {
            code: "orm_schema_mismatch",
            ..
        })
    ));
    let result: Result<Vec<Post>, _> = records.find(Filter::all(), Default::default()).await;
    assert!(matches!(
        result,
        Err(DbError::Configuration {
            code: "orm_schema_mismatch",
            ..
        })
    ));
}

async fn database() -> (Database, tempfile::TempDir) {
    database_with_keys(ProjectKeySource::unavailable()).await
}

async fn database_with_keys(key_source: ProjectKeySource) -> (Database, tempfile::TempDir) {
    crate::tests::fixtures::reset_engine();
    let directory = tempfile::tempdir().unwrap();
    let binding = DbBinding::cold_start("orm_fixture");
    let migration_backend = zeroship_migrate_sqlite::SqliteBackend::open(
        &directory
            .path()
            .join(format!("zs-{}.sqlite", binding.app_id())),
        &directory.path().join("migrations.sqlite"),
    )
    .unwrap();
    // A rendered migration unit can contain table and index statements.
    // Apply it through the migration backend's batch executor.
    migration_backend
        .restore_schema_sqlite(
            &table_statements("main", &zeroship_migrate_sqlite::DIALECT).join("\n"),
        )
        .await
        .unwrap();
    migration_backend
        .actor()
        .exec("CREATE UNIQUE INDEX unique_title ON posts(title)")
        .await
        .unwrap();
    drop(migration_backend);
    let schema = <posts::Entity as Entity>::schema().clone();
    let database = Database::connect(
        binding,
        crate::ConnectOptions::new(
            directory.path().join("control.sqlite").to_string_lossy(),
            key_source,
        ),
        vec![("posts".into(), schema)],
    )
    .await
    .unwrap();
    (database, directory)
}

fn table_statements(app: &str, dialect: &zeroship_migrate::DialectId) -> Vec<String> {
    let effective_policy = zeroship_migrate::effective_policy_from_charter_toml(
        zeroship_migrate_server::policy::CONFINED_CEILING_TOML,
    )
    .unwrap();
    zeroship_migrate::render_ir_envelope_sql_statements(
        zeroship_migrate::shipping_vendors(),
        include_str!("../../tests/fixtures/orm-migration.json"),
        dialect,
        &zeroship_migrate::PreviewOpts {
            default_schema: app.into(),
            owner_app: app.into(),
            effective_policy,
        },
    )
    .unwrap()
    .1
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
    let posts = db.entity::<posts::Entity>().unwrap();
    let inserted: Post = posts
        .insert(NewPost {
            title: "hello".into(),
        })
        .await
        .unwrap();
    assert!(inserted.id.starts_with("post_"));
    let updated: Post = posts
        .update(
            posts::id.eq(inserted.id.clone()).unwrap(),
            posts::title.set("edited").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.title, "edited");
    assert!(updated.version > inserted.version);
    assert_eq!(
        posts
            .find::<Post>(Filter::all(), FindOptions::default())
            .await
            .unwrap()
            .len(),
        1
    );
    posts
        .delete::<Post>(posts::id.eq(inserted.id.clone()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        posts
            .find::<Post>(Filter::all(), FindOptions::default())
            .await
            .unwrap()
            .is_empty()
    );
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
            .find::<Post>(Filter::all(), FindOptions::default())
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
    assert!(
        escaped
            .find(value!({}), value!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("settled")
    );
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
        .entity::<posts::Entity>()
        .unwrap()
        .find::<Post>(Filter::all(), FindOptions::default())
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
    let route =
        CapturedRoute::pool_for_tests("another_app", crate::sql::compile::SqlDialect::Sqlite);
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

/// A host-defined backend proves that ORM operations do not depend on built-in
/// concrete types. The delegated sessions still talk to the required databases.
#[derive(Debug)]
struct RegisteredBackend {
    inner: BackendHandle,
    queries: Rc<Cell<usize>>,
    transactions: Rc<Cell<usize>>,
}
#[async_trait::async_trait(?Send)]
impl crate::executor::ScopedExecutor for RegisteredBackend {
    fn dialect(&self) -> crate::sql::compile::SqlDialect {
        self.inner.dialect()
    }
    async fn prepare_for_app(&self, app_id: &str) -> Result<(), DbError> {
        self.inner.prepare_for_app(app_id).await
    }
    async fn query(
        &self,
        app_id: &str,
        schema: &crate::sql::SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        self.queries.set(self.queries.get() + 1);
        self.inner.query(app_id, schema, sql, params).await
    }
    async fn exec(
        &self,
        app_id: &str,
        schema: &crate::sql::SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        self.inner.exec(app_id, schema, sql, params).await
    }
    async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &crate::sql::SchemaName,
        begin: crate::error::BeginIntent,
    ) -> Result<crate::driver::Session, crate::error::OpenSessionError> {
        self.transactions.set(self.transactions.get() + 1);
        self.inner.open_tx_session(app_id, schema, begin).await
    }
}
#[async_trait::async_trait(?Send)]
impl crate::protection::Catalog for RegisteredBackend {
    async fn introspect_schema(
        &self,
        app_id: &str,
    ) -> Result<crate::sql::catalog::LiveSchema, DbError> {
        self.inner.introspect_schema(app_id).await
    }
}
#[async_trait::async_trait(?Send)]
impl crate::search::Search for RegisteredBackend {
    async fn vector_search(
        &self,
        session: Option<&crate::driver::Session>,
        request: crate::search::VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.inner.vector_search(session, request).await
    }
    async fn spatial_near(
        &self,
        session: Option<&crate::driver::Session>,
        request: crate::search::SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.inner.spatial_near(session, request).await
    }
}
impl crate::protection::Protection for RegisteredBackend {
    fn key_store(&self) -> &crate::encryption::KeyStore {
        self.inner.key_store()
    }
}

impl crate::backend::Backend for RegisteredBackend {
    fn publishes_committed_changes(&self) -> bool {
        self.inner.publishes_committed_changes()
    }
}

async fn exercise_registered_backend(mut db: Database) {
    let queries = Rc::new(Cell::new(0));
    let transactions = Rc::new(Cell::new(0));
    db.backend = BackendHandle::new(Rc::new(RegisteredBackend {
        inner: db.backend.clone(),
        queries: queries.clone(),
        transactions: transactions.clone(),
    }));
    exercise_native_models(&db).await;
    joins::exercise(&db).await;
    let committed = db
        .transaction(|tx| async move {
            let posts = tx.entity::<posts::Entity>()?;
            let row: Post = posts
                .insert(NewPost {
                    title: "registered committed".into(),
                })
                .await?;
            let found: Vec<Post> = posts
                .find(posts::id.eq(row.id.clone())?, Default::default())
                .await?;
            assert_eq!(found[0].id, row.id);
            let nested: Result<(), DbError> = tx
                .transaction(|inner| async move {
                    let _: Post = inner
                        .entity::<posts::Entity>()?
                        .insert(NewPost {
                            title: "registered savepoint".into(),
                        })
                        .await?;
                    Err(DbError::validation(
                        "rollback_probe",
                        "rollback nested insert",
                    ))
                })
                .await;
            assert!(nested.is_err());
            Ok(row.id)
        })
        .await
        .unwrap();
    let posts = db.entity::<posts::Entity>().unwrap();
    let found: Vec<Post> = posts
        .find(posts::id.eq(committed).unwrap(), Default::default())
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert!(
        posts
            .find::<Post>(
                posts::title.eq("registered savepoint").unwrap(),
                Default::default()
            )
            .await
            .unwrap()
            .is_empty()
    );
    let rolled_back: Result<(), DbError> = db
        .transaction(|tx| async move {
            let _: Post = tx
                .entity::<posts::Entity>()?
                .insert(NewPost {
                    title: "registered rollback".into(),
                })
                .await?;
            Err(DbError::validation("rollback_probe", "rollback insert"))
        })
        .await;
    assert!(rolled_back.is_err());
    assert!(
        posts
            .find::<Post>(
                posts::title.eq("registered rollback").unwrap(),
                Default::default()
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        queries.get() > 0,
        "autocommit must use the registered driver"
    );
    assert!(
        transactions.get() > 0,
        "transactions must use the registered driver"
    );
}

#[compio::test]
async fn changing_backend_registration_refuses_an_open_transaction() {
    let (db, _directory) = database().await;
    let result = db
        .transaction(|mut tx| async move {
            tx.backend = BackendHandle::new(Rc::new(RegisteredBackend {
                inner: tx.backend.clone(),
                queries: Rc::new(Cell::new(0)),
                transactions: Rc::new(Cell::new(0)),
            }));
            let _: Post = tx
                .entity::<posts::Entity>()?
                .insert(NewPost {
                    title: "wrong backend".into(),
                })
                .await?;
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert!(
        db.entity::<posts::Entity>()
            .unwrap()
            .find::<Post>(Filter::all(), Default::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

#[compio::test]
async fn independent_databases_keep_schema_policy_and_transactions_isolated() {
    let (first, _first_files) = database().await;
    let (second, _second_files) = database().await;
    assert_eq!(first.binding(), second.binding());
    first.install_mask_policy(value!({})).unwrap();
    second
        .install_mask_policy(value!({ "reader": ["pii"] }))
        .unwrap();
    let read_first = first
        .entity::<posts::Entity>()
        .unwrap()
        .find::<Post>(Filter::all(), Default::default());
    second.context().with(|| {
        crate::schema_cache::with_mut(|cache| {
            cache.insert_one(
                second.binding(),
                "only_second",
                value!({ "flag": { "type": "boolean" } }),
            );
        })
    });
    assert!(first.collection("only_second").is_err());
    assert!(second.collection("only_second").is_ok());
    assert!(read_first.await.unwrap().is_empty());

    let other = second.clone();
    let result: Result<(), DbError> = compio::time::timeout(
        std::time::Duration::from_secs(10),
        first.transaction(|first_tx| async move {
            let _: Post = first_tx
                .entity::<posts::Entity>()?
                .insert(NewPost {
                    title: "rolled back".into(),
                })
                .await?;
            other
                .transaction(|second_tx| async move {
                    let _: Post = second_tx
                        .entity::<posts::Entity>()?
                        .insert(NewPost {
                            title: "committed".into(),
                        })
                        .await?;
                    Ok(())
                })
                .await?;
            Err(DbError::internal("roll back the first database"))
        }),
    )
    .await
    .expect("independent transaction admission must not block");
    assert!(result.is_err());
    assert!(
        first
            .entity::<posts::Entity>()
            .unwrap()
            .find::<Post>(Filter::all(), Default::default())
            .await
            .unwrap()
            .is_empty()
    );
    let rows = second
        .entity::<posts::Entity>()
        .unwrap()
        .find::<Post>(Filter::all(), Default::default())
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["committed"]
    );
}

#[compio::test]
async fn cancelled_transaction_cleans_up_its_own_context() {
    let (first, _first_files) = database().await;
    let (second, _second_files) = database().await;
    let (ready_tx, ready_rx) = flume::bounded(1);
    let mut cancelled = Box::pin(first.transaction(|tx| async move {
        let _: Post = tx
            .entity::<posts::Entity>()?
            .insert(NewPost {
                title: "cancelled".into(),
            })
            .await?;
        ready_tx.send_async(()).await.unwrap();
        std::future::pending::<()>().await;
        Ok(())
    }));
    std::future::poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        if ready_rx.try_recv().is_ok() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    // Drop from another database's context: recovery must retain its origin.
    second.context().with(|| drop(cancelled));
    let _: Post = second
        .entity::<posts::Entity>()
        .unwrap()
        .insert(NewPost {
            title: "survives".into(),
        })
        .await
        .unwrap();
    compio::time::timeout(
        std::time::Duration::from_secs(10),
        first.transaction(|tx| async move {
            assert!(
                tx.entity::<posts::Entity>()?
                    .find::<Post>(Filter::all(), Default::default())
                    .await?
                    .is_empty()
            );
            Ok(())
        }),
    )
    .await
    .expect("cancelled transaction must release its admission")
    .unwrap();
    assert_eq!(
        second
            .entity::<posts::Entity>()
            .unwrap()
            .find::<Post>(Filter::all(), Default::default())
            .await
            .unwrap()[0]
            .title,
        "survives"
    );
}
