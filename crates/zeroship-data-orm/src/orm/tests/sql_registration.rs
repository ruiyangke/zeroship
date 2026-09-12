use super::fixtures::CollectionFixture;
use super::*;
use crate::sql::{
    compiler::{CompileError, SqlCompiler, SqliteCompiler},
    registration::{SqlRegistration, SqlStorageCodecs},
    statement::StorageType,
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
    let file = owner.sqlite_file.as_ref().unwrap();
    let backend =
        crate::backend_selection::open_sqlite_backend(file, ProjectKeySource::unavailable())
            .await
            .unwrap();
    let compiler = SqliteCompiler;
    let mut effective = compiler.support();
    effective.conditional_conflict_update = false;
    let registration = SqlRegistration::new(
        "fixture-without-conditional-upsert",
        crate::sql::compile::SqlDialect::Sqlite,
        compiler,
        FixtureCodecs,
        effective,
    )
    .unwrap();
    let backend = crate::backend::BackendHandle::with_sql(Rc::new(backend), registration).unwrap();
    let constrained = Database::new(
        owner.database.context.clone(),
        owner.database.binding.clone(),
        backend,
    );
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
    owner.close().await;
}
