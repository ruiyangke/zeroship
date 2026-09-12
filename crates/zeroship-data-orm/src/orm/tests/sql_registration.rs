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
    fn storage_type(&self, _: &Value) -> Result<StorageType, CompileError> {
        Ok(StorageType::Text)
    }

    fn encode(&self, _: StorageType, value: Value) -> Result<Value, CompileError> {
        Ok(value)
    }

    fn decode(&self, _: StorageType, value: Value) -> Result<Value, CompileError> {
        Ok(value)
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
    assert!(error.to_string().contains("returning projections"));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
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
