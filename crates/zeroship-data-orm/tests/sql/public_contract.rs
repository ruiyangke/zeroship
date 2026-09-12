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

#[test]
fn typed_plan_output_debug_does_not_disclose_bound_values() {
    use zeroship_data_orm::sql::{
        BindBudget, ColumnValue, Ident, IdentRole, Insert, Literal, Returning, WriteValue,
        render::postgres,
    };
    let secret = "private-typed-plan-value";
    let insert = Insert::builder(
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        Returning::nothing(),
        BindBudget::POSTGRES,
    )
    .row(vec![ColumnValue::new(
        Ident::parse_as("title", IdentRole::Column).unwrap(),
        WriteValue::Bind(Literal::text(secret).unwrap()),
    )])
    .build()
    .unwrap();
    let query: zeroship_data_orm::sql::compiler::CompiledQuery =
        postgres::render_insert(&insert).unwrap();
    assert!(
        !format!("{query:?}").contains(secret),
        "typed plan output debug disclosed a bound value"
    );
    assert_eq!(query.params(), &[Value::from(secret)]);
}

#[test]
fn postgres_compiler_counts_pagination_in_statement_bind_limit() {
    use zeroship_data_orm::sql::{
        BindBudget, Ident, IdentRole, Literal, MembershipOp, Operand, Predicate, ProjectedField,
        Projection, Select, literal::MAX_MEMBERSHIP_LIST_LEN, render::postgres,
    };
    let column = Ident::parse_as("id", IdentRole::Column).unwrap();
    let limit = BindBudget::POSTGRES.max();
    let plan = |binds: usize| {
        let values: Vec<_> = (0..binds).map(|value| Literal::Int(value as i64)).collect();
        let predicates = values
            .chunks(MAX_MEMBERSHIP_LIST_LEN)
            .map(|values| {
                Predicate::membership(
                    Operand::column(column.clone()),
                    MembershipOp::In,
                    values.iter().cloned().map(Some).collect(),
                )
                .unwrap()
            })
            .collect();
        Select::builder(
            Ident::parse_as("entries", IdentRole::Collection).unwrap(),
            Projection::rows(vec![ProjectedField::column(column.clone()).unwrap()]).unwrap(),
        )
        .filter(Predicate::Or(predicates))
        .build()
        .unwrap()
    };
    let query = postgres::render_select(&plan(limit - 2)).unwrap();
    assert_eq!(query.params().len(), limit);
    assert!(
        postgres::render_select(&plan(limit - 1)).is_err(),
        "pagination exceeded the statement bind limit without a compiler error"
    );
}
