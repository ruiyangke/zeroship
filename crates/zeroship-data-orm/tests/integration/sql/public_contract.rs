use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::{
    error::DbError,
    sql::{
        compiler::{CompiledQuery, ParameterType},
        registration::SqlRegistration,
        statement::{
            Assignment, Expression, Insert, InsertParts, ReturnedColumn, Statement, StorageType,
            Table, Upsert, UpsertParts,
        },
        Ident, IdentRole,
    },
    value, Value,
};

fn ident(name: &str, role: IdentRole) -> Ident {
    Ident::parse_as(name, role).unwrap()
}

fn table(namespace: &str) -> Table {
    Table::new(
        SchemaName::new(namespace).unwrap(),
        ident("entries", IdentRole::Collection),
        [
            (ident("id", IdentRole::StoredColumn), StorageType::Text),
            (ident("title", IdentRole::StoredColumn), StorageType::Text),
            (
                ident("payload", IdentRole::StoredColumn),
                StorageType::Bytes,
            ),
        ],
    )
    .unwrap()
}

fn compile_insert(registration: &SqlRegistration, title: Value, payload: Value) -> CompiledQuery {
    let table = table("app-demo");
    let id = table.column("id").unwrap();
    let title_column = table.column("title").unwrap();
    let payload_column = table.column("payload").unwrap();
    registration
        .compile(Statement::Insert(
            Insert::new(InsertParts {
                table,
                columns: vec![id.clone(), title_column.clone(), payload_column],
                rows: vec![vec![
                    Expression::Bind(value!("entry")),
                    Expression::Bind(title),
                    Expression::Bind(payload),
                ]],
                returning: vec![
                    ReturnedColumn {
                        column: id,
                        alias: None,
                    },
                    ReturnedColumn {
                        column: title_column,
                        alias: None,
                    },
                ],
                insert_generated_identity: false,
            })
            .unwrap(),
        ))
        .unwrap()
}

#[test]
fn compilers_share_schema_identity_and_keep_values_out_of_sql() {
    let hostile = "'); DROP TABLE entries; --";
    for registration in [SqlRegistration::postgres(), SqlRegistration::sqlite()] {
        let query = compile_insert(&registration, value!(hostile), Value::Bytes(vec![251, 252]));
        assert!(query.sql().contains("\"app-demo\".\"entries\""));
        assert!(!query.sql().contains(hostile));
        assert_eq!(
            query.params(),
            &[
                value!("entry"),
                Value::Bytes(vec![251, 252]),
                value!(hostile)
            ]
        );
        let debug = format!("{query:?}");
        assert!(!debug.contains(hostile));
        assert!(!debug.contains("251"));
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
fn compiled_output_exposes_types_and_transfers_owned_buffers() {
    let bytes = vec![0, 1, 255];
    let pointer = bytes.as_ptr();
    let query = CompiledQuery::new(
        "SELECT $1, $2, $3".into(),
        vec![
            Value::Bytes(bytes),
            Value::TimestampMicros(0),
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
    let (sql, values) = query.into_parts();
    assert_eq!(sql, "SELECT $1, $2, $3");
    assert_eq!(values[0].as_bytes().unwrap().as_ptr(), pointer);
}

fn compile_upsert(registration: &SqlRegistration, reverse: bool) -> CompiledQuery {
    let table = table("app_upsert");
    let id = table.column("id").unwrap();
    let title = table.column("title").unwrap();
    let mut insert = vec![
        Assignment {
            column: id.clone(),
            value: Expression::Bind(value!("entry")),
        },
        Assignment {
            column: title.clone(),
            value: Expression::Bind(value!("title")),
        },
    ];
    if reverse {
        insert.reverse();
    }
    registration
        .compile(Statement::Upsert(
            Upsert::new(UpsertParts {
                table,
                insert,
                conflict: vec![id],
                update: vec![Assignment {
                    column: title.clone(),
                    value: Expression::Incoming(title),
                }],
                condition: None,
                returning: Vec::new(),
                insert_generated_identity: false,
            })
            .unwrap(),
        ))
        .unwrap()
}

#[test]
fn upsert_normalizes_unordered_insert_assignments() {
    for registration in [SqlRegistration::postgres(), SqlRegistration::sqlite()] {
        let forward = compile_upsert(&registration, false);
        let reverse = compile_upsert(&registration, true);
        assert_eq!(forward.sql(), reverse.sql());
        assert_eq!(forward.params(), reverse.params());
    }
}
