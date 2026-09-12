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
            query.sql().contains("\"app-demo\".\"entries\""),
            "{}",
            query.sql()
        );
        assert!(!query.sql().contains(hostile));
        assert_eq!(
            query.params(),
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

#[test]
fn compiled_query_debug_does_not_disclose_bound_values() {
    let namespace = SchemaName::new("app_debug").unwrap();
    let secret = "private-column-value";
    let input =
        value!({"id": "entry", "title": secret, "payload": Value::Bytes(vec![251, 252, 253])});
    let schema = value!({
        "id": {"type": "string", "primaryKey": true},
        "title": {"type": "string"},
        "payload": {"type": "bytes"}
    });
    for dialect in [compile::SqlDialect::Postgres, compile::SqlDialect::Sqlite] {
        let query =
            compile::build_insert_with_dialect(&namespace, "entries", &schema, &input, dialect)
                .unwrap();
        let debug = format!("{query:?}");
        assert!(
            !debug.contains(secret),
            "compiled query debug disclosed text"
        );
        assert!(
            !debug.contains("251"),
            "compiled query debug disclosed binary data"
        );
    }
}

#[test]
fn compiled_output_exposes_bind_types_and_transfers_owned_buffers() {
    use zeroship_data_orm::sql::compiler::{CompiledQuery, ParameterType};
    let bytes = vec![0, 1, 255];
    let pointer = bytes.as_ptr();
    let query = CompiledQuery::new(
        "SELECT $1, $2, $3".into(),
        vec![
            Value::Bytes(bytes),
            Value::Timestamp(0),
            Value::Json("\"text\"".into()),
        ],
    );
    assert_eq!(
        query.parameter_types().collect::<Vec<_>>(),
        vec![
            ParameterType::Bytes,
            ParameterType::Timestamp,
            ParameterType::Json,
        ]
    );
    let debug = format!("{query:?}");
    assert!(debug.contains("Bytes") && debug.contains("Timestamp") && debug.contains("Json"));
    let (sql, values) = query.into_parts();
    assert_eq!(sql, "SELECT $1, $2, $3");
    assert_eq!(values[0].as_bytes().unwrap().as_ptr(), pointer);
    assert_eq!(values[1], Value::Timestamp(0));
    assert_eq!(values[2], Value::Json("\"text\"".into()));
}
