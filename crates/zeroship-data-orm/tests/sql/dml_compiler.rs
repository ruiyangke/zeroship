use zeroship_data_orm::{
    sql::{
        compiler::{CompileError, PostgresCompiler, Requirements, SqlCompiler, SqliteCompiler},
        registration::{SqlRegistration, SqlStorageCodecs},
        statement::{
            Assignment, Comparison, Expression, Insert, InsertParts, ResolvedJoin, ResolvedOperand,
            ResolvedPredicate, ResolvedPredicateValue, ReturnedColumn, SelectParts,
            SelectStatement, SelectedExpression, SpatialNearParts, SpatialNearStatement, Statement,
            StorageType, Table, Upsert, UpsertParts, VectorSearchParts, VectorSearchStatement,
        },
        CompareOp, Ident, IdentRole, JoinKind, SchemaName,
    },
    value::Value,
};

fn table() -> Table {
    Table::new(
        SchemaName::new("app-upserts").unwrap(),
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        [
            ("id", StorageType::Integer),
            ("payload", StorageType::Bytes),
            ("document", StorageType::Json),
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

fn search_table() -> Table {
    Table::aliased(
        SchemaName::new("app-search").unwrap(),
        Ident::parse_as("places", IdentRole::Collection).unwrap(),
        Ident::parse_as("source", IdentRole::Alias).unwrap(),
        [
            ("id", StorageType::Text),
            ("label", StorageType::Text),
            ("embedding", StorageType::Vector),
            ("location", StorageType::GeoPoint),
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

#[test]
fn registered_compilers_own_vector_search_syntax() {
    let statement = |query| {
        let table = search_table();
        Statement::VectorSearch(
            VectorSearchStatement::new(VectorSearchParts {
                projection: vec![ReturnedColumn {
                    column: table.column("id").unwrap(),
                    alias: None,
                }],
                identity: table.column("id").unwrap(),
                vector: table.column("embedding").unwrap(),
                query,
                metric: zeroship_data_orm::sql::descriptors::VectorMetric::Cosine,
                predicate: ResolvedPredicate::Compare {
                    lhs: ResolvedOperand::Column(table.column("label").unwrap()),
                    op: CompareOp::Eq,
                    rhs: ResolvedPredicateValue::Bind {
                        storage: StorageType::Text,
                        value: Value::from("open"),
                    },
                },
                limit: 5,
                table,
            })
            .unwrap(),
        )
    };

    let postgres = PostgresCompiler
        .compile(
            statement(zeroship_data_orm::value!([1.0, 2.0])),
            &PostgresCompiler.support(),
        )
        .unwrap();
    assert!(postgres.sql().contains(" <=> $1::vector AS \"_distance\""));
    assert!(postgres
        .sql()
        .contains("ORDER BY \"source\".\"embedding\" <=> $1::vector"));
    assert_eq!(
        postgres.params(),
        &[
            zeroship_data_orm::value!([1.0, 2.0]),
            Value::from("open"),
            Value::from(5)
        ]
    );

    let sqlite = SqliteCompiler
        .compile(
            statement(Value::Bytes(vec![0; 8])),
            &SqliteCompiler.support(),
        )
        .unwrap();
    assert!(sqlite
        .sql()
        .contains("vec_distance_cosine(\"source\".\"embedding\", $1)"));
    assert_eq!(sqlite.params().len(), 3);
}

#[test]
fn registered_compilers_choose_the_spatial_execution_statement() {
    let statement = || {
        let table = search_table();
        Statement::SpatialNear(
            SpatialNearStatement::new(SpatialNearParts {
                projection: vec![ReturnedColumn {
                    column: table.column("id").unwrap(),
                    alias: None,
                }],
                spatial: table.column("location").unwrap(),
                point: zeroship_data_orm::value!({"lat":51.5,"lng":-0.1}),
                radius_m: 1000.0,
                predicate: ResolvedPredicate::Const(true),
                limit: 5,
                table,
            })
            .unwrap(),
        )
    };

    let postgres = PostgresCompiler
        .compile(statement(), &PostgresCompiler.support())
        .unwrap();
    assert!(postgres.sql().contains("ST_DWithin"));
    assert!(postgres.sql().contains("ST_Distance"));

    let sqlite = SqliteCompiler
        .compile(statement(), &SqliteCompiler.support())
        .unwrap();
    assert!(!sqlite.sql().contains("ST_DWithin"));
    assert!(!sqlite.sql().contains("LIMIT"));
    assert!(sqlite.sql().starts_with("SELECT \"source\".\"id\""));
}

fn insert_parts(table: &Table) -> InsertParts {
    InsertParts {
        table: table.clone(),
        columns: vec![
            table.column("payload").unwrap(),
            table.column("id").unwrap(),
        ],
        rows: vec![vec![
            Expression::Bind(Value::Bytes(vec![1, 2, 3])),
            Expression::Bind(Value::from(7)),
        ]],
        returning: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: None,
        }],
        insert_generated_identity: true,
    }
}

#[test]
fn column_comparisons_do_not_consume_the_bind_budget() {
    let source = Table::aliased(
        SchemaName::new("app-reads").unwrap(),
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        Ident::parse_as("source", IdentRole::Alias).unwrap(),
        [("id", StorageType::Integer)].map(|(name, storage)| {
            (
                Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                storage,
            )
        }),
    )
    .unwrap();
    let joined = Table::aliased(
        SchemaName::new("app-reads").unwrap(),
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        Ident::parse_as("joined", IdentRole::Alias).unwrap(),
        [("id", StorageType::Integer)].map(|(name, storage)| {
            (
                Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                storage,
            )
        }),
    )
    .unwrap();
    let statement = || {
        Statement::Select(
            SelectStatement::new(SelectParts {
                table: source.clone(),
                joins: vec![ResolvedJoin {
                    kind: JoinKind::Inner,
                    table: joined.clone(),
                    on: ResolvedPredicate::Compare {
                        lhs: ResolvedOperand::Column(source.column("id").unwrap()),
                        op: CompareOp::Eq,
                        rhs: ResolvedPredicateValue::Operand(ResolvedOperand::Column(
                            joined.column("id").unwrap(),
                        )),
                    },
                }],
                projection: vec![SelectedExpression {
                    expression: ResolvedOperand::Column(source.column("id").unwrap()),
                    alias: Ident::parse_as("id", IdentRole::Alias).unwrap(),
                }],
                predicate: ResolvedPredicate::Const(true),
                group_by: Vec::new(),
                having: ResolvedPredicate::Const(true),
                order_by: Vec::new(),
                limit: Some(1),
                offset: Some(0),
                distinct: false,
            })
            .unwrap(),
        )
    };
    let requirements = Requirements::for_statement(&statement());
    assert_eq!(requirements.bind_parameters, 2);
    let mut support = SqliteCompiler.support();
    support.max_bind_parameters = requirements.bind_parameters;
    let compiled = SqliteCompiler.compile(statement(), &support).unwrap();
    assert_eq!(compiled.params(), &[Value::from(1), Value::from(0)]);
}

#[test]
fn a_join_cannot_reference_a_source_that_has_not_been_introduced() {
    let aliased = |alias: &str| {
        Table::aliased(
            SchemaName::new("app-reads").unwrap(),
            Ident::parse_as("entries", IdentRole::Collection).unwrap(),
            Ident::parse_as(alias, IdentRole::Alias).unwrap(),
            [("id", StorageType::Integer)].map(|(name, storage)| {
                (
                    Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                    storage,
                )
            }),
        )
        .unwrap()
    };
    let source = aliased("source");
    let joined = aliased("joined");
    let forward = aliased("forward");
    let compare = |left: &Table, right: &Table| ResolvedPredicate::Compare {
        lhs: ResolvedOperand::Column(left.column("id").unwrap()),
        op: CompareOp::Eq,
        rhs: ResolvedPredicateValue::Operand(ResolvedOperand::Column(right.column("id").unwrap())),
    };
    let statement = SelectStatement::new(SelectParts {
        table: source.clone(),
        joins: vec![
            ResolvedJoin {
                kind: JoinKind::Inner,
                table: joined.clone(),
                on: compare(&source, &forward),
            },
            ResolvedJoin {
                kind: JoinKind::Inner,
                table: forward.clone(),
                on: compare(&joined, &forward),
            },
        ],
        projection: vec![SelectedExpression {
            expression: ResolvedOperand::Column(source.column("id").unwrap()),
            alias: Ident::parse_as("id", IdentRole::Alias).unwrap(),
        }],
        predicate: ResolvedPredicate::Const(true),
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
    });
    assert!(statement.is_err());
}

#[test]
fn an_aggregate_select_rejects_an_ungrouped_column() {
    let source = Table::aliased(
        SchemaName::new("app-reads").unwrap(),
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        Ident::parse_as("source", IdentRole::Alias).unwrap(),
        [
            ("id", StorageType::Integer),
            ("revision", StorageType::Integer),
        ]
        .map(|(name, storage)| {
            (
                Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                storage,
            )
        }),
    )
    .unwrap();
    let statement = SelectStatement::new(SelectParts {
        table: source.clone(),
        joins: Vec::new(),
        projection: vec![
            SelectedExpression {
                expression: ResolvedOperand::Column(source.column("id").unwrap()),
                alias: Ident::parse_as("id", IdentRole::Alias).unwrap(),
            },
            SelectedExpression {
                expression: ResolvedOperand::Aggregate {
                    function: zeroship_data_orm::sql::AggregateFunc::Sum,
                    column: Some(source.column("revision").unwrap()),
                    distinct: false,
                },
                alias: Ident::parse_as("total", IdentRole::Alias).unwrap(),
            },
        ],
        predicate: ResolvedPredicate::Const(true),
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
    });
    assert!(statement.is_err());
}

#[test]
fn insert_columns_are_canonical_and_rows_follow_their_columns() {
    let table = table();
    let statement = Statement::Insert(Insert::new(insert_parts(&table)).unwrap());
    let postgres = PostgresCompiler
        .compile(statement, &PostgresCompiler.support())
        .unwrap();
    assert_eq!(
        postgres.sql(),
        r#"INSERT INTO "app-upserts"."entries" ("id", "payload") OVERRIDING SYSTEM VALUE VALUES ($1, $2) RETURNING "id""#
    );
    assert_eq!(
        postgres.params(),
        &[Value::from(7), Value::Bytes(vec![1, 2, 3])]
    );

    let sqlite = SqliteCompiler
        .compile(
            Statement::Insert(Insert::new(insert_parts(&table)).unwrap()),
            &SqliteCompiler.support(),
        )
        .unwrap();
    assert_eq!(
        sqlite.sql(),
        r#"INSERT INTO "app-upserts"."entries" ("id", "payload") VALUES ($1, $2) RETURNING "id""#
    );
}

#[test]
fn insert_rejects_foreign_columns_and_invalid_row_shapes() {
    let own = table();
    let foreign = table();
    let mut input = insert_parts(&own);
    input.columns[0] = foreign.column("payload").unwrap();
    assert!(Insert::new(input).is_err());

    let mut input = insert_parts(&own);
    input.rows[0].pop();
    assert!(Insert::new(input).is_err());

    let mut input = insert_parts(&own);
    input.rows[0][0] = Expression::Incoming(own.column("payload").unwrap());
    assert!(Insert::new(input).is_err());
}

fn parts(table: &Table) -> UpsertParts {
    UpsertParts {
        table: table.clone(),
        insert: vec![Assignment {
            column: table.column("id").unwrap(),
            value: Expression::Bind(Value::from(7)),
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
            column: table.column("id").unwrap(),
            op: CompareOp::Eq,
            value: Value::from(7),
        }),
        returning: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: None,
        }],
        insert_generated_identity: false,
    }
}

#[test]
fn source_membership_cannot_be_forged_with_the_same_table_name() {
    let own = table();
    let foreign = table();
    for position in ["insert", "update", "conflict", "condition", "returning"] {
        let mut input = parts(&own);
        let foreign = foreign.column("id").unwrap();
        match position {
            "insert" => input.insert[0].column = foreign,
            "update" => input.update[0].value = Expression::Incoming(foreign),
            "conflict" => input.conflict[0] = foreign,
            "condition" => input.condition.as_mut().unwrap().column = foreign,
            "returning" => input.returning[0].column = foreign,
            _ => unreachable!(),
        }
        assert!(
            Upsert::new(input).is_err(),
            "accepted foreign reference in {position}"
        );
    }
    assert!(Upsert::new(parts(&own)).is_ok());
}

#[test]
fn incoming_values_and_native_types_are_checked_in_their_clause() {
    let table = table();
    let mut input = parts(&table);
    input.insert[0].value = Expression::Incoming(table.column("id").unwrap());
    assert!(Upsert::new(input).is_err());
    let mut input = parts(&table);
    input.insert[0].value = Expression::Bind(Value::Bytes(vec![1]));
    assert!(Upsert::new(input).is_err());
    let mut input = parts(&table);
    input.insert.push(Assignment {
        column: table.column("id").unwrap(),
        value: Expression::Null,
    });
    assert!(Upsert::new(input).is_err());
}

#[test]
fn compilation_rechecks_effective_support_and_total_bind_requirements() {
    let table = table();
    for compiler in [&PostgresCompiler as &dyn SqlCompiler, &SqliteCompiler] {
        let statement = || Statement::Upsert(Upsert::new(parts(&table)).unwrap());
        let requirements = Requirements::for_statement(&statement());
        assert_eq!(requirements.bind_parameters, 3);
        let mut effective = compiler.support();
        effective.max_bind_parameters = requirements.bind_parameters;
        assert_eq!(
            compiler
                .compile(statement(), &effective)
                .unwrap()
                .params()
                .len(),
            3
        );
        effective.max_bind_parameters -= 1;
        assert!(matches!(
            compiler.compile(statement(), &effective),
            Err(CompileError::BindLimitExceeded { .. })
        ));
        let mut effective = compiler.support();
        effective.conditional_conflict_update = false;
        assert!(compiler.check(&requirements, &effective).is_err());
        assert!(compiler.compile(statement(), &effective).is_err());
        effective = compiler.support();
        effective.returning = false;
        assert!(compiler.compile(statement(), &effective).is_err());
    }
}

#[test]
fn default_sql_null_and_json_null_are_distinct() {
    let table = table();
    let mut input = parts(&table);
    input.insert.push(Assignment {
        column: table.column("payload").unwrap(),
        value: Expression::Null,
    });
    input.insert.push(Assignment {
        column: table.column("document").unwrap(),
        value: Expression::Bind(Value::Json("null".into())),
    });
    input.insert.push(Assignment {
        column: table.column("revision").unwrap(),
        value: Expression::Default,
    });
    let query = PostgresCompiler
        .compile(
            Statement::Upsert(Upsert::new(input).unwrap()),
            &PostgresCompiler.support(),
        )
        .unwrap();
    assert!(
        query.sql().contains("VALUES ($1, $2, NULL, DEFAULT)"),
        "{}",
        query.sql()
    );
    assert_eq!(query.params()[0], Value::Json("null".into()));
    let mut input = parts(&table);
    input.insert[0].value = Expression::Default;
    assert!(matches!(
        SqliteCompiler.compile(
            Statement::Upsert(Upsert::new(input).unwrap()),
            &SqliteCompiler.support()
        ),
        Err(CompileError::Unsupported(_))
    ));
}

#[test]
fn statement_compilation_moves_buffers_and_keeps_values_out_of_sql() {
    let table = table();
    for compiler in [&PostgresCompiler as &dyn SqlCompiler, &SqliteCompiler] {
        let payload = b"'); DROP TABLE entries; --".to_vec();
        let pointer = payload.as_ptr();
        let mut input = parts(&table);
        input.insert.push(Assignment {
            column: table.column("payload").unwrap(),
            value: Expression::Bind(Value::Bytes(payload)),
        });
        let query = compiler
            .compile(
                Statement::Upsert(Upsert::new(input).unwrap()),
                &compiler.support(),
            )
            .unwrap();
        assert_eq!(query.params()[1].as_bytes().unwrap().as_ptr(), pointer);
        assert!(query
            .sql()
            .starts_with("INSERT INTO \"app-upserts\".\"entries\""));
        assert!(!query.sql().contains("DROP TABLE"));
        assert!(!format!("{query:?}").contains("DROP TABLE"));
    }
}

#[derive(Clone, Copy)]
struct DownstreamCompiler;

impl SqlCompiler for DownstreamCompiler {
    fn support(&self) -> zeroship_data_orm::sql::compiler::SqlSupport {
        PostgresCompiler.support()
    }

    fn check(
        &self,
        requirements: &Requirements,
        effective: &zeroship_data_orm::sql::compiler::SqlSupport,
    ) -> Result<(), CompileError> {
        PostgresCompiler.check(requirements, effective)
    }

    fn compile(
        &self,
        statement: Statement,
        effective: &zeroship_data_orm::sql::compiler::SqlSupport,
    ) -> Result<zeroship_data_orm::sql::compiler::CompiledQuery, CompileError> {
        PostgresCompiler.compile(statement, effective)
    }

    fn compile_identity_allocation(
        &self,
        request: zeroship_data_orm::sql::statement::IdentityRequest,
        effective: &zeroship_data_orm::sql::compiler::SqlSupport,
    ) -> Result<zeroship_data_orm::sql::compiler::IdentityPlan, CompileError> {
        PostgresCompiler.compile_identity_allocation(request, effective)
    }
}

#[derive(Clone, Copy)]
struct DownstreamCodecs;

impl SqlStorageCodecs for DownstreamCodecs {
    fn storage_type(&self, _: &Value) -> Result<StorageType, CompileError> {
        Ok(StorageType::Integer)
    }

    fn encode(&self, _: StorageType, value: Value) -> Result<Value, CompileError> {
        Ok(value)
    }

    fn decode(&self, _: StorageType, value: Value) -> Result<Value, CompileError> {
        Ok(value)
    }
}

#[test]
fn a_downstream_compiler_and_codecs_register_without_a_vendor_enum_arm() {
    let registration = SqlRegistration::new(
        "fixture-sql",
        zeroship_data_orm::sql::compile::SqlDialect::Postgres,
        DownstreamCompiler,
        DownstreamCodecs,
        DownstreamCompiler.support(),
    )
    .unwrap();
    assert_ne!(
        registration.identity(),
        SqlRegistration::builtin(zeroship_data_orm::sql::compile::SqlDialect::Postgres).identity()
    );
    assert_eq!(
        registration
            .compile(Statement::Upsert(Upsert::new(parts(&table())).unwrap()))
            .unwrap()
            .params()
            .len(),
        3
    );
}

#[test]
fn identity_allocation_sql_belongs_to_the_registered_compiler() {
    use zeroship_data_orm::sql::{compiler::IdentityReadPlan, statement::IdentityRequest};

    let table = table();
    let request = || IdentityRequest::new(table.clone(), table.column("id").unwrap(), 2).unwrap();
    let postgres = SqlRegistration::builtin(zeroship_data_orm::sql::compile::SqlDialect::Postgres)
        .compile_identity_allocation(request())
        .unwrap();
    assert!(postgres.reservation.is_none());
    assert!(matches!(postgres.allocation, IdentityReadPlan::Rows(_)));

    let sqlite = SqlRegistration::builtin(zeroship_data_orm::sql::compile::SqlDialect::Sqlite)
        .compile_identity_allocation(request())
        .unwrap();
    assert!(sqlite.reservation.is_some());
    assert!(matches!(
        sqlite.allocation,
        IdentityReadPlan::MaximumAndCounter { .. }
    ));
}
