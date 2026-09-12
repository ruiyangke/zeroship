use super::fixtures::CollectionFixture;
use super::*;
use crate::sql::{
    compiler::{
        CompileError, CompiledQuery, Requirements, SqlCompiler, SqlSupport, SqliteCompiler,
    },
    registration::{SqlRegistration, SqlStorageCodecs},
    statement::{Statement, StorageType},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone, Copy)]
struct FixtureCodecs;

impl SqlStorageCodecs for FixtureCodecs {
    fn storage_type(&self, definition: &Value) -> Result<StorageType, CompileError> {
        SqlRegistration::builtin(crate::sql::compile::SqlDialect::Sqlite).storage_type(definition)
    }

    fn encode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
        SqlRegistration::builtin(crate::sql::compile::SqlDialect::Sqlite).encode(storage, value)
    }

    fn decode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
        SqlRegistration::builtin(crate::sql::compile::SqlDialect::Sqlite).decode(storage, value)
    }
}

#[test]
fn sqlite_registration_owns_boolean_vector_and_geographic_storage() {
    let registration = SqlRegistration::builtin(crate::sql::compile::SqlDialect::Sqlite);
    let boolean = registration
        .storage_type(&value!({"type":"boolean"}))
        .unwrap();
    assert_eq!(
        registration.encode(boolean, Value::Bool(true)).unwrap(),
        Value::from(1)
    );

    let vector = registration
        .storage_type(&value!({"type":"vector"}))
        .unwrap();
    let encoded = registration.encode(vector, value!([1.0, -0.5])).unwrap();
    assert!(matches!(encoded, Value::Bytes(_)));
    assert_eq!(
        registration.decode(vector, encoded).unwrap(),
        value!([1.0, -0.5])
    );

    let point = registration
        .storage_type(&value!({"type":"geoPoint"}))
        .unwrap();
    let encoded = registration
        .encode(point, value!({"lat":37.0,"lng":-122.0}))
        .unwrap();
    assert!(matches!(encoded, Value::Bytes(_)));
    assert_eq!(
        registration.decode(point, encoded).unwrap(),
        value!({"lat":37.0,"lng":-122.0})
    );
}

#[test]
fn sqlite_registration_rejects_malformed_encoded_spatial_values() {
    let registration = SqlRegistration::builtin(crate::sql::compile::SqlDialect::Sqlite);
    let vector = registration
        .storage_type(&value!({"type":"vector"}))
        .unwrap();
    let point = registration
        .storage_type(&value!({"type":"geoPoint"}))
        .unwrap();

    for value in [Value::Bytes(Vec::new()), Value::Bytes(vec![0; 3])] {
        assert!(registration.encode(vector, value.clone()).is_err());
        assert!(registration.decode(vector, value).is_err());
    }
    let invalid_point = Value::Bytes(f64::NAN.to_le_bytes().repeat(2));
    assert!(registration.encode(point, invalid_point.clone()).is_err());
    assert!(registration.decode(point, invalid_point).is_err());
}

async fn constrained_database(
    owner: &CollectionFixture,
    identity: &str,
    configure: impl FnOnce(&mut crate::sql::compiler::SqlSupport),
) -> Database {
    let compiler = SqliteCompiler;
    let mut effective = compiler.support();
    configure(&mut effective);
    let registration = SqlRegistration::new(
        identity,
        crate::sql::compile::SqlDialect::Sqlite,
        compiler,
        FixtureCodecs,
        effective,
    )
    .unwrap();
    database_with_registration(owner, registration).await
}

async fn database_with_registration(
    owner: &CollectionFixture,
    registration: SqlRegistration,
) -> Database {
    let file = owner.sqlite_file.as_ref().unwrap();
    let backend =
        crate::backend_selection::open_sqlite_backend(file, ProjectKeySource::unavailable())
            .await
            .unwrap();
    let backend = crate::backend::BackendHandle::with_sql(Rc::new(backend), registration).unwrap();
    Database::new(
        owner.database.context.clone(),
        owner.database.binding.clone(),
        backend,
    )
}

#[derive(Clone)]
struct CountingCompiler(Arc<AtomicUsize>);

impl SqlCompiler for CountingCompiler {
    fn support(&self) -> SqlSupport {
        SqliteCompiler.support()
    }

    fn check(
        &self,
        requirements: &Requirements,
        effective: &SqlSupport,
    ) -> Result<(), CompileError> {
        SqliteCompiler.check(requirements, effective)
    }

    fn compile(
        &self,
        statement: Statement,
        effective: &SqlSupport,
    ) -> Result<CompiledQuery, CompileError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        SqliteCompiler.compile(statement, effective)
    }

    fn compile_identity_allocation(
        &self,
        request: crate::sql::statement::IdentityRequest,
        effective: &SqlSupport,
    ) -> Result<crate::sql::compiler::IdentityPlan, CompileError> {
        SqliteCompiler.compile_identity_allocation(request, effective)
    }
}

async fn assert_empty(owner: &CollectionFixture) {
    let Output::Rows { rows, .. } = owner
        .database
        .collection("records")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert!(rows.is_empty());
}

#[compio::test]
async fn insert_is_refused_by_the_registered_compiler_before_identity_allocation() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let compiler = CountingCompiler(calls.clone());
    let mut support = compiler.support();
    support.returning = false;
    let registration = SqlRegistration::new(
        "fixture-without-returning",
        crate::sql::compile::SqlDialect::Sqlite,
        compiler,
        FixtureCodecs,
        support,
    )
    .unwrap();
    let constrained = database_with_registration(&owner, registration).await;
    let error = constrained
        .collection("records")
        .unwrap()
        .insert(value!({"label":"blocked"}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("returning projections"),
        "{error}"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_empty(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn generated_identity_is_refused_before_the_write_frame_and_allocation() {
    let mut owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let mut migration: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/identity-migration.json"
    ))
    .unwrap();
    migration["ops"][0]["columns"][0]["identity"]["always"] = serde_json::json!(false);
    owner
        .replace_from_migration("records", &migration.to_string())
        .await;
    let constrained =
        constrained_database(&owner, "fixture-without-identity-allocation", |support| {
            support.identity_allocation = false;
        })
        .await;

    let error = constrained
        .collection("records")
        .unwrap()
        .insert(value!({"label":"blocked"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("generated identity allocation"));
    assert_empty(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn conditional_upsert_is_refused_before_the_write_frame_and_row_mutation() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({
            "label":{"type":"string","unique":true},
            "secret":{"type":"string","encrypted":true}
        }),
    )
    .await;
    let constrained =
        constrained_database(&owner, "fixture-without-conditional-upsert", |support| {
            support.conditional_conflict_update = false
        })
        .await;
    let error = constrained
        .collection("records")
        .unwrap()
        .execute(Operation::Upsert {
            document: value!({"label":"same", "secret":"private"}),
            conflict_fields: value!(["label"]),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("conditional conflict updates"));
    assert_empty(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn update_one_is_refused_by_the_registered_compiler_before_row_mutation() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let Output::Rows { rows, .. } = owner
        .database
        .collection("records")
        .unwrap()
        .insert(value!({"label":"original"}))
        .await
        .unwrap()
    else {
        panic!("expected inserted row")
    };
    let id = rows[0]["id"].clone();
    let constrained = constrained_database(&owner, "fixture-update-without-returning", |support| {
        support.returning = false
    })
    .await;

    let error = constrained
        .collection("records")
        .unwrap()
        .update(value!({"id":id}), value!({"label":"changed"}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("returning projections"),
        "{error}"
    );

    let Output::Rows { rows, .. } = owner
        .database
        .collection("records")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert_eq!(rows[0]["label"], value!("original"));
    owner.close().await;
}

#[compio::test]
async fn relational_read_is_refused_by_the_registered_compiler() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let constrained = constrained_database(&owner, "fixture-without-relational-reads", |support| {
        support.relational_reads = false
    })
    .await;
    let source = ReadSource::new("records", "source");
    let joined = ReadSource::new("records", "joined");
    let mut query = ReadQuery::new(source.clone());
    query.joins.push(ReadJoin {
        kind: crate::sql::JoinKind::Inner,
        source: joined.clone(),
        on: crate::sql::Predicate::compare(
            crate::sql::Operand::Path(source.column("id").unwrap()),
            crate::sql::CompareOp::Eq,
            crate::sql::Operand::Path(joined.column("id").unwrap()),
        ),
    });
    query.projection.push(ReadProjection::Row {
        output: "record".into(),
        source: source.alias,
        fields: Some(vec!["label".into()]),
        optional: false,
    });

    let error = constrained.read(query).await.unwrap_err();
    assert!(error.to_string().contains("relational reads"), "{error}");
    owner.close().await;
}

#[compio::test]
async fn aggregate_read_is_refused_by_the_registered_compiler() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let constrained = constrained_database(&owner, "fixture-without-aggregate-reads", |support| {
        support.aggregate_reads = false
    })
    .await;
    let source = ReadSource::new("records", "source");
    let mut query = ReadQuery::new(source.clone());
    query.projection.push(ReadProjection::Scalar {
        output: "records".into(),
        expression: crate::sql::Operand::Aggregate(
            crate::sql::AggregateRef::over_path(
                crate::sql::AggregateFunc::Count,
                source.column("id").unwrap(),
                false,
            )
            .unwrap(),
        ),
    });

    let error = constrained.read(query).await.unwrap_err();
    assert!(error.to_string().contains("aggregate reads"), "{error}");
    owner.close().await;
}

#[compio::test]
async fn dynamic_find_uses_the_registered_compiler() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let compiler = CountingCompiler(calls.clone());
    let registration = SqlRegistration::new(
        "count-dynamic-find-compilation",
        crate::sql::compile::SqlDialect::Sqlite,
        compiler.clone(),
        FixtureCodecs,
        compiler.support(),
    )
    .unwrap();
    let database = database_with_registration(&owner, registration).await;

    database
        .collection("records")
        .unwrap()
        .find(value!({"label":"missing"}), value!({}))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    owner.close().await;
}

#[compio::test]
async fn dynamic_find_refuses_a_non_filterable_field() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({"secret":{"type":"string","filterable":false}}),
    )
    .await;
    let error = owner
        .database
        .collection("records")
        .unwrap()
        .find(value!({"secret":"hidden"}), value!({}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not filterable"), "{error}");
    owner.close().await;
}

#[compio::test]
async fn typed_update_and_delete_use_the_registered_compiler() {
    let owner = CollectionFixture::sqlite("posts", posts::Entity::schema().clone()).await;
    let Output::Rows { rows, .. } = owner
        .database
        .collection("posts")
        .unwrap()
        .insert(value!({"title":"original"}))
        .await
        .unwrap()
    else {
        panic!("expected inserted row")
    };
    let id = rows[0]["id"].as_str().unwrap().to_owned();
    let calls = Arc::new(AtomicUsize::new(0));
    let compiler = CountingCompiler(calls.clone());
    let registration = SqlRegistration::new(
        "count-typed-write-compilation",
        crate::sql::compile::SqlDialect::Sqlite,
        compiler.clone(),
        FixtureCodecs,
        compiler.support(),
    )
    .unwrap();
    let database = database_with_registration(&owner, registration).await;
    let posts = database.entity::<posts::Entity>().unwrap();

    let updated: Option<Post> = posts
        .update(
            posts::id.eq(id.clone()).unwrap(),
            posts::title.set("changed".to_owned()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(updated.unwrap().title, "changed");
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    calls.store(0, Ordering::Relaxed);
    let deleted: Option<Post> = posts.delete(posts::id.eq(id).unwrap()).await.unwrap();
    assert_eq!(deleted.unwrap().title, "changed");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    owner.close().await;
}

#[compio::test]
async fn dynamic_count_is_refused_by_the_registered_compiler() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let constrained = constrained_database(&owner, "dynamic-count-without-aggregates", |support| {
        support.aggregate_reads = false
    })
    .await;

    let error = constrained
        .collection("records")
        .unwrap()
        .count(value!({}), value!({}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("aggregate reads"), "{error}");
    owner.close().await;
}

#[compio::test]
async fn dynamic_distinct_uses_the_registered_compiler() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let compiler = CountingCompiler(calls.clone());
    let registration = SqlRegistration::new(
        "count-dynamic-distinct-compilation",
        crate::sql::compile::SqlDialect::Sqlite,
        compiler.clone(),
        FixtureCodecs,
        compiler.support(),
    )
    .unwrap();
    let database = database_with_registration(&owner, registration).await;

    database
        .collection("records")
        .unwrap()
        .execute(Operation::Distinct {
            field: "label".into(),
            filter: value!({}),
            options: value!({}),
        })
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    owner.close().await;
}

#[compio::test]
async fn dynamic_aggregate_uses_the_registered_compiler() {
    let owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let compiler = CountingCompiler(calls.clone());
    let registration = SqlRegistration::new(
        "count-dynamic-aggregate-compilation",
        crate::sql::compile::SqlDialect::Sqlite,
        compiler.clone(),
        FixtureCodecs,
        compiler.support(),
    )
    .unwrap();
    let database = database_with_registration(&owner, registration).await;

    database
        .collection("records")
        .unwrap()
        .execute(Operation::Aggregate {
            pipeline: value!([{"$group":{"records":{"$count":true}}}]),
            options: value!({}),
        })
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    owner.close().await;
}

#[compio::test]
async fn search_is_refused_by_the_registered_compiler() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({"embedding":{"type":"vector","vectorDims":2}}),
    )
    .await;
    let constrained = constrained_database(&owner, "fixture-without-vector-search", |support| {
        support.vector_search = false
    })
    .await;

    let error = constrained
        .collection("records")
        .unwrap()
        .execute(Operation::Search {
            arguments: value!({"vector":[1.0,0.0]}),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("vector search"), "{error}");
    owner.close().await;
}

#[compio::test]
async fn search_limit_is_checked_during_preparation() {
    let owner = CollectionFixture::sqlite(
        "records",
        value!({"embedding":{"type":"vector","vectorDims":2}}),
    )
    .await;
    let error = owner
        .database
        .collection("records")
        .unwrap()
        .execute(Operation::Search {
            arguments: value!({"vector":[1.0,0.0],"k":501}),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("search.k"), "{error}");
    owner.close().await;
}
