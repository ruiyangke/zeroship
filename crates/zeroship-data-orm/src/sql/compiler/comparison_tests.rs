use super::*;
use crate::sql::{
    CompareOp, Ident, IdentRole, SchemaName,
    statement::{
        Assignment, Comparison, Expression, Insert, InsertParts, ReturnedColumn, Statement,
        StorageType, Table, Upsert, UpsertParts,
    },
};
use crate::{Value, value};

fn table(namespace: &str, storage: StorageType) -> Table {
    Table::new(
        SchemaName::new(namespace).unwrap(),
        Ident::parse_as("comparison_entries", IdentRole::Collection).unwrap(),
        [
            ("id", StorageType::Integer),
            ("payload", storage),
            ("revision", StorageType::Integer),
        ]
        .map(|(name, storage)| {
            (
                Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                storage,
            )
        }),
    )
    .unwrap()
}

fn upsert(table: &Table, op: CompareOp, value: Value) -> Result<Upsert, CompileError> {
    Upsert::new(UpsertParts {
        table: table.clone(),
        insert: vec![Assignment {
            column: table.column("id").unwrap(),
            value: Expression::Bind(value!(1)),
        }],
        conflict: vec![table.column("id").unwrap()],
        update: vec![Assignment {
            column: table.column("revision").unwrap(),
            value: Expression::Increment {
                column: table.column("revision").unwrap(),
                step: 1,
            },
        }],
        condition: Some(Comparison {
            column: table.column("payload").unwrap(),
            op,
            value,
        }),
        returning: vec![ReturnedColumn {
            column: table.column("revision").unwrap(),
            alias: None,
        }],
        insert_generated_identity: false,
    })
}

#[test]
fn conflict_conditions_reject_nonportable_comparison_operators() {
    for (storage, value) in [
        (StorageType::Boolean, value!(true)),
        (StorageType::Bytes, Value::Bytes(vec![1])),
        (
            StorageType::exact_decimal(10, 2).unwrap(),
            Value::Decimal("1.00".into()),
        ),
        (StorageType::Json, value!({"key":1})),
        (StorageType::Vector, value!([1.0, 2.0])),
        (StorageType::GeoPoint, value!({"lat":1.0,"lng":2.0})),
    ] {
        for op in [CompareOp::Lt, CompareOp::Lte, CompareOp::Gt, CompareOp::Gte] {
            assert!(
                upsert(&table("public", storage), op, value.clone()).is_err(),
                "{storage:?} {op:?}"
            );
        }
        if matches!(storage, StorageType::Vector | StorageType::GeoPoint) {
            for op in [CompareOp::Eq, CompareOp::Ne] {
                assert!(
                    upsert(&table("public", storage), op, value.clone()).is_err(),
                    "{storage:?} {op:?}"
                );
            }
        }
    }
}

fn real_insert(value: Value) -> Result<Insert, CompileError> {
    let table = table("public", StorageType::Real);
    Insert::new(InsertParts {
        columns: vec![table.column("payload").unwrap()],
        table,
        rows: vec![vec![Expression::Bind(value)]],
        returning: Vec::new(),
        insert_generated_identity: false,
    })
}

#[test]
fn real_parameters_reject_nonportable_unsigned_values_before_compilation() {
    for value in [Value::from(i64::MAX as u64 + 1), Value::from(u64::MAX)] {
        assert!(
            real_insert(value.clone()).is_err(),
            "nonportable Real insert: {value}"
        );
        assert!(upsert(&table("public", StorageType::Real), CompareOp::Eq, value).is_err());
    }
}

#[test]
fn real_parameters_accept_the_shared_driver_number_range() {
    for value in [
        value!(0_u64),
        Value::from(i64::MAX as u64),
        value!(i64::MIN),
        value!(1.5),
        value!(f64::MAX),
    ] {
        for compiler in [&PostgresCompiler as &dyn SqlCompiler, &SqliteCompiler] {
            let query = compiler
                .compile(
                    Statement::Insert(real_insert(value.clone()).unwrap()),
                    &compiler.support(),
                )
                .unwrap();
            assert_eq!(query.params(), std::slice::from_ref(&value));
        }
    }
}

async fn exercise_json_condition(
    compiler: &dyn SqlCompiler,
    session: &dyn crate::driver::DriverSession,
    namespace: &str,
    json_type: &str,
) {
    session.exec(&format!("CREATE TABLE {namespace}.comparison_entries (id BIGINT PRIMARY KEY, payload {json_type}, revision BIGINT NOT NULL DEFAULT 0)"), &[]).await.unwrap();
    session
        .exec(
            &format!("INSERT INTO {namespace}.comparison_entries (id, payload) VALUES ($1, $2)"),
            &[
                value!(1),
                Value::Json(r#"{"first":1,"second":[true,null]}"#.into()),
            ],
        )
        .await
        .unwrap();
    let table = table(namespace, StorageType::Json);
    let mut revision = 0;
    for (op, json, matches) in [
        (
            CompareOp::Eq,
            r#"{ "second": [true,null], "first": 1.00e0 }"#,
            true,
        ),
        (
            CompareOp::Ne,
            r#"{"second":[true,null],"first":1.0}"#,
            false,
        ),
        (CompareOp::Ne, r#"{"first":"1","second":[true,null]}"#, true),
        (CompareOp::Eq, r#"{"first":2,"second":[true,null]}"#, false),
    ] {
        let query = compiler
            .compile(
                Statement::Upsert(upsert(&table, op, Value::Json(json.into())).unwrap()),
                &compiler.support(),
            )
            .unwrap();
        let rows = session.query(query.sql(), query.params()).await.unwrap();
        assert_eq!(
            rows.len(),
            usize::from(matches),
            "{op:?} {json}; SQL: {}",
            query.sql()
        );
        if matches {
            revision += 1;
            assert_eq!(rows[0]["revision"], value!(revision));
        }
        let stored = session
            .query(
                &format!("SELECT revision FROM {namespace}.comparison_entries WHERE id = 1"),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(stored[0]["revision"], value!(revision));
    }
}

#[compio::test]
async fn sqlite_conflict_conditions_compare_json_structurally() {
    use crate::driver::{Driver, LeaseKind};
    let directory = tempfile::tempdir().unwrap();
    let backend = crate::backend::sqlite::SqliteBackend::open(
        directory.path().join("control.sqlite"),
        std::sync::Arc::new(crate::cdc::broker::BrokerChangeSink),
        crate::encryption::ProjectKeySource::unavailable(),
    )
    .await
    .unwrap();
    let driver = backend
        .connection_driver(
            "comparison",
            &crate::sql::SchemaName::new("comparison").unwrap(),
        )
        .await
        .unwrap();
    let session = driver.acquire(LeaseKind::Autocommit).await.unwrap();
    exercise_json_condition(&SqliteCompiler, &*session, "comparison", "TEXT").await;
}

#[compio::test]
async fn postgres_conflict_conditions_compare_json_structurally() {
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
    let pool = compio_postgres::Pool::connect(&postgres.url(), 1)
        .await
        .unwrap();
    let connection = pool.acquire().await.unwrap();
    exercise_json_condition(&PostgresCompiler, &connection, "public", "JSONB").await;
    drop(connection);
    pool.close().await;
}

fn comparison_projection(
    storage: StorageType,
    value: Value,
    foreign_column: bool,
    grouped: bool,
) -> Result<crate::sql::statement::SelectStatement, CompileError> {
    use crate::sql::statement::{
        ResolvedOperand, ResolvedPredicate, RowLock, SelectParts, SelectStatement,
        SelectedExpression,
    };
    let make_table = || {
        Table::aliased(
            SchemaName::new("public").unwrap(),
            Ident::parse_as("comparison_entries", IdentRole::Collection).unwrap(),
            Ident::parse_as("source", IdentRole::Alias).unwrap(),
            [(
                Ident::parse_as("payload", IdentRole::Column).unwrap(),
                storage,
            )],
        )
        .unwrap()
    };
    let table = make_table();
    let column = if foreign_column {
        make_table().column("payload").unwrap()
    } else {
        table.column("payload").unwrap()
    };
    let mut projection = vec![SelectedExpression {
        expression: ResolvedOperand::Comparison(Comparison {
            column,
            op: CompareOp::Eq,
            value,
        }),
        alias: Ident::parse_as("matches", IdentRole::Alias).unwrap(),
    }];
    if grouped {
        projection.push(SelectedExpression {
            expression: ResolvedOperand::Aggregate {
                function: crate::sql::AggregateFunc::Count,
                column: None,
                distinct: false,
            },
            alias: Ident::parse_as("count", IdentRole::Alias).unwrap(),
        });
    }
    SelectStatement::new(SelectParts {
        table,
        joins: Vec::new(),
        projection,
        predicate: ResolvedPredicate::Const(true),
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: RowLock::None,
    })
}

#[test]
fn comparison_projections_validate_sources_storage_grouping_and_bind_budgets() {
    for compiler in [&PostgresCompiler as &dyn SqlCompiler, &SqliteCompiler] {
        for (storage, value) in [
            (StorageType::Text, value!("Ada")),
            (StorageType::Integer, value!(9_007_199_254_740_993_i64)),
        ] {
            let statement = Statement::Select(
                comparison_projection(storage, value.clone(), false, false).unwrap(),
            );
            assert_eq!(Requirements::for_statement(&statement).bind_parameters, 1);
            let query = compiler.compile(statement, &compiler.support()).unwrap();
            assert_eq!(query.params(), std::slice::from_ref(&value));
            let mut unsupported = compiler.support();
            unsupported.max_bind_parameters = 0;
            let statement =
                Statement::Select(comparison_projection(storage, value, false, false).unwrap());
            assert!(matches!(
                compiler.compile(statement, &unsupported),
                Err(CompileError::BindLimitExceeded { .. })
            ));
        }
    }
    assert!(comparison_projection(StorageType::Text, value!(1), false, false).is_err());
    assert!(comparison_projection(StorageType::Text, Value::Null, false, false).is_err());
    assert!(comparison_projection(StorageType::Text, value!("Ada"), true, false).is_err());
    assert!(comparison_projection(StorageType::Text, value!("Ada"), false, true).is_err());
}
