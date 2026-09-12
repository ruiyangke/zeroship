use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::{Value, error::DbError, sql::compile, value};

#[test]
fn compiler_accepts_shared_schema_identity_and_native_orm_values() {
    let namespace = SchemaName::new("app-demo").unwrap();
    let hostile = "'); DROP TABLE entries; --";
    let schema =
        value!({"id": {"type": "string", "primaryKey": true}, "title": {"type": "string"}});
    let input = value!({"id": "entry", "title": hostile});
    for dialect in [compile::SqlDialect::Postgres, compile::SqlDialect::Sqlite] {
        let query =
            compile::build_insert_with_dialect(&namespace, "entries", &schema, &input, dialect)
                .unwrap();
        assert!(
            query.sql.contains("\"app-demo\".\"entries\""),
            "{}",
            query.sql
        );
        assert!(!query.sql.contains(hostile));
        assert_eq!(
            query.params,
            vec![Value::from("entry"), Value::from(hostile)]
        );
    }
}

#[test]
fn invalid_shared_schema_identity_keeps_the_orm_validation_error() {
    let error: DbError = SchemaName::new("app.public").unwrap_err().into();
    assert!(matches!(
        error,
        DbError::ValidationFailed {
            code: "invalid_collection",
            ..
        }
    ));
}
