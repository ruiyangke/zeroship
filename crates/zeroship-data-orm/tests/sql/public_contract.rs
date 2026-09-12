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

#[test]
fn production_upsert_normalizes_unordered_column_input() {
    let namespace = SchemaName::new("app_upsert_shape").unwrap();
    let schema = value!({
        "id": {"type": "string", "primaryKey": true},
        "email": {"type": "string"},
        "title": {"type": "string"},
        "payload": {"type": "bytes"},
    });
    let forwards = value!({"id": "entry", "email": "key", "title": "text", "payload": Value::Bytes(vec![255])});
    let backwards = value!({"payload": Value::Bytes(vec![255]), "title": "text", "email": "key", "id": "entry"});
    let conflict = value!(["email"]);
    for dialect in [compile::SqlDialect::Postgres, compile::SqlDialect::Sqlite] {
        let build = |input| {
            zeroship_data_orm::crud::upsert::build_upsert_with_dialect(
                &namespace, "entries", &schema, input, &conflict, dialect,
            )
            .unwrap()
        };
        let first = build(forwards.clone());
        let second = build(backwards.clone());
        assert_eq!(
            first.sql(),
            second.sql(),
            "input map order changed upsert SQL"
        );
        assert_eq!(first.params(), second.params());
    }
}

#[test]
fn production_upsert_transfers_native_buffers_into_bindings() {
    let namespace = SchemaName::new("app_upsert_buffers").unwrap();
    let schema = value!({
        "id": {"type": "string", "primaryKey": true},
        "payload": {"type": "bytes"},
        "title": {"type": "string"},
        "document": {"type": "json"},
    });
    for dialect in [compile::SqlDialect::Postgres, compile::SqlDialect::Sqlite] {
        let bytes = vec![0, 255, 128];
        let pointer = bytes.as_ptr();
        let title = String::from("owned text");
        let title_pointer = title.as_ptr();
        let document = String::from("{\"key\":true}");
        let document_pointer = document.as_ptr();
        let input = Value::Object(
            [
                ("id".into(), Value::from("entry")),
                ("payload".into(), Value::Bytes(bytes)),
                ("title".into(), Value::String(title)),
                ("document".into(), Value::Json(document)),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(input["payload"].as_bytes().unwrap().as_ptr(), pointer);
        let query = zeroship_data_orm::crud::upsert::build_upsert_with_dialect(
            &namespace,
            "entries",
            &schema,
            input,
            &value!(["id"]),
            dialect,
        )
        .unwrap();
        let bound = query.params().iter().find_map(Value::as_bytes).unwrap();
        assert_eq!(
            bound.as_ptr(),
            pointer,
            "upsert copied its native binary input"
        );
        let text = query
            .params()
            .iter()
            .filter_map(Value::as_str)
            .find(|value| *value == "owned text")
            .unwrap();
        assert_eq!(text.as_ptr(), title_pointer);
        let json = query
            .params()
            .iter()
            .find_map(|value| match value {
                Value::Json(value) => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(json.as_ptr(), document_pointer);
    }
}

#[test]
fn production_upsert_budget_includes_generated_assignments_and_guard() {
    use zeroship_data_orm::sql::lifecycle::{AssignedValue, ColumnAssignment, WriteAssignments};
    let limit = zeroship_data_orm::sql::BindBudget::SQLITE.max();
    let namespace = SchemaName::new("app_upsert_budget").unwrap();
    let mut schema = value!({"id": {"type": "integer", "primaryKey":true}, "revision": {"type": "integer"}});
    let mut input = value!({"id": 1});
    for i in 1..limit - 1 {
        let field = format!("field_{i}");
        schema[field.as_str()] = value!({"type": "integer"});
        input[field.as_str()] = Value::from(i);
    }
    let assignments = WriteAssignments {
        columns: vec![ColumnAssignment {
            column: "revision".into(),
            value: AssignedValue::Increment(1),
        }],
    };
    let build = |guard| {
        zeroship_data_orm::crud::upsert::build_upsert_with_assignments(
            &namespace,
            "entries",
            &schema,
            input.clone(),
            &value!(["id"]),
            compile::SqlDialect::Sqlite,
            &assignments,
            guard,
        )
    };
    assert_eq!(build(None).unwrap().params().len(), limit);
    assert_eq!(
        build(Some([("id".into(), Value::from(1))].into())).unwrap_err(),
        compile::QueryError::from(zeroship_data_orm::sql::compiler::CompileError::BindLimitExceeded { limit }),
    );
}
