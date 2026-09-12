use zeroship_data_orm::{
    sql::{
        compiler::{
            CompileError, CompiledQuery, IdentityPlan, PostgresCompiler, Requirements, SqlCompiler,
            SqlSupport, SqliteCompiler,
        },
        registration::{SqlFamily, SqlRegistration, SqlStorageCodecs},
        statement::{
            ArithmeticOperator, ArrayOperator, Assignment, Comparison, Delete, DeleteParts,
            Expression, IdentityRequest, Insert, InsertParts, MutationScope, ResolvedJoin,
            ResolvedOperand, ResolvedPredicate, ResolvedPredicateValue, ReturnedColumn,
            SelectParts, SelectStatement, SelectedExpression, SpatialNearParts,
            SpatialNearStatement, Statement, StorageType, Table, Update, UpdateParts, Upsert,
            UpsertParts, VectorSearchParts, VectorSearchStatement,
        },
        CompareOp, Direction, Ident, IdentRole, JoinKind, MembershipOp, NullOrder, SchemaName,
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

fn aliased_table() -> Table {
    Table::aliased(
        SchemaName::new("app-upserts").unwrap(),
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        Ident::parse_as("source", IdentRole::Alias).unwrap(),
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
            ("amount", StorageType::Decimal),
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

fn select_with_predicate(
    table: Table,
    predicate: ResolvedPredicate,
) -> Result<SelectStatement, CompileError> {
    SelectStatement::new(SelectParts {
        projection: vec![SelectedExpression {
            expression: ResolvedOperand::Column(table.column("id").unwrap()),
            alias: Ident::parse_as("id", IdentRole::Alias).unwrap(),
        }],
        predicate,
        joins: Vec::new(),
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: zeroship_data_orm::sql::statement::RowLock::None,
        table,
    })
}

fn scalar_select(
    storage: StorageType,
    configure: impl FnOnce(&mut SelectParts),
) -> Result<SelectStatement, CompileError> {
    let table = Table::aliased(
        SchemaName::new("app-selects").unwrap(),
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        Ident::parse_as("source", IdentRole::Alias).unwrap(),
        [
            (
                Ident::parse_as("id", IdentRole::StoredColumn).unwrap(),
                StorageType::Integer,
            ),
            (
                Ident::parse_as("value", IdentRole::StoredColumn).unwrap(),
                storage,
            ),
        ],
    )
    .unwrap();
    let value = ResolvedOperand::Column(table.column("value").unwrap());
    let mut parts = SelectParts {
        table,
        joins: Vec::new(),
        projection: vec![SelectedExpression {
            expression: value,
            alias: Ident::parse_as("value", IdentRole::Alias).unwrap(),
        }],
        predicate: ResolvedPredicate::Const(true),
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: zeroship_data_orm::sql::statement::RowLock::None,
    };
    configure(&mut parts);
    SelectStatement::new(parts)
}

#[test]
fn resolved_statements_reject_empty_connectives() {
    for predicate in [
        ResolvedPredicate::And(Vec::new()),
        ResolvedPredicate::Or(Vec::new()),
    ] {
        assert_eq!(
            select_with_predicate(aliased_table(), predicate).unwrap_err(),
            CompileError::InvalidStatement("predicate connective cannot be empty".into())
        );
    }
}

#[test]
fn resolved_selects_reject_backend_dependent_ordering() {
    for storage in [
        StorageType::Boolean,
        StorageType::Bytes,
        StorageType::Decimal,
        StorageType::Json,
        StorageType::Vector,
        StorageType::GeoPoint,
    ] {
        let result = scalar_select(storage, |parts| {
            parts.order_by.push(zeroship_data_orm::sql::statement::ResolvedOrder {
                expression: parts.projection[0].expression.clone(),
                direction: Direction::Ascending,
                nulls: NullOrder::Last,
            });
        });
        assert_eq!(
            result.unwrap_err(),
            CompileError::InvalidStatement("order by requires portable ordered storage".into())
        );
    }
    for storage in [
        StorageType::Integer,
        StorageType::Real,
        StorageType::Text,
        StorageType::Timestamp,
    ] {
        assert!(scalar_select(storage, |parts| {
            parts.order_by.push(zeroship_data_orm::sql::statement::ResolvedOrder {
                expression: parts.projection[0].expression.clone(),
                direction: Direction::Ascending,
                nulls: NullOrder::Last,
            });
        })
        .is_ok());
    }
}

#[test]
fn resolved_selects_reject_backend_dependent_grouping_and_distinctness() {
    for storage in [
        StorageType::Decimal,
        StorageType::Json,
        StorageType::Vector,
        StorageType::GeoPoint,
    ] {
        let grouped = scalar_select(storage, |parts| {
            parts.group_by.push(parts.projection[0].expression.clone());
        });
        assert_eq!(
            grouped.unwrap_err(),
            CompileError::InvalidStatement("group by requires portable equality storage".into())
        );

        let distinct = scalar_select(storage, |parts| parts.distinct = true);
        assert_eq!(
            distinct.unwrap_err(),
            CompileError::InvalidStatement("distinct requires portable equality storage".into())
        );
    }
    for storage in [
        StorageType::Boolean,
        StorageType::Integer,
        StorageType::Real,
        StorageType::Text,
        StorageType::Bytes,
        StorageType::Timestamp,
    ] {
        assert!(scalar_select(storage, |parts| {
            parts.group_by.push(parts.projection[0].expression.clone());
        })
        .is_ok());
        assert!(scalar_select(storage, |parts| parts.distinct = true).is_ok());
    }
}

#[test]
fn count_distinct_rejects_backend_dependent_equality() {
    for storage in [
        StorageType::Decimal,
        StorageType::Json,
        StorageType::Vector,
        StorageType::GeoPoint,
    ] {
        let result = scalar_select(storage, |parts| {
            let column = match &parts.projection[0].expression {
                ResolvedOperand::Column(column) => column.clone(),
                _ => unreachable!(),
            };
            parts.projection[0].expression = ResolvedOperand::Aggregate {
                function: zeroship_data_orm::sql::AggregateFunc::Count,
                column: Some(column),
                distinct: true,
            };
        });
        assert_eq!(
            result.unwrap_err(),
            CompileError::InvalidStatement(
                "distinct aggregate requires portable equality storage".into()
            )
        );
    }
}

#[test]
fn resolved_predicates_refuse_non_portable_storage_operators() {
    let table = search_table();
    for (field, value) in [
        ("amount", Value::Decimal("1.5".into())),
        ("embedding", Value::Bytes(vec![0; 8])),
        ("location", Value::Bytes(vec![0; 16])),
    ] {
        let predicate = ResolvedPredicate::Compare {
            lhs: ResolvedOperand::Column(table.column(field).unwrap()),
            op: CompareOp::Gt,
            rhs: ResolvedPredicateValue::Bind {
                storage: table.column(field).unwrap().storage(),
                value,
            },
        };
        assert!(
            select_with_predicate(table.clone(), predicate).is_err(),
            "{field}"
        );
    }

    for field in ["embedding", "location"] {
        let column = table.column(field).unwrap();
        let values = vec![match field {
            "embedding" => Value::Bytes(vec![0; 8]),
            _ => Value::Bytes(vec![0; 16]),
        }];
        let predicate = ResolvedPredicate::Membership {
            lhs: ResolvedOperand::Column(column),
            op: MembershipOp::In,
            values,
        };
        assert!(
            select_with_predicate(table.clone(), predicate).is_err(),
            "{field}"
        );
    }
}

#[test]
fn sqlite_uses_structural_json_equality_for_filters() {
    let table = aliased_table();
    let document = table.column("document").unwrap();
    let predicate = ResolvedPredicate::Compare {
        lhs: ResolvedOperand::Column(document.clone()),
        op: CompareOp::Eq,
        rhs: ResolvedPredicateValue::Bind {
            storage: StorageType::Json,
            value: Value::Json(r#"{"b":2,"a":1}"#.into()),
        },
    };
    let compiled = SqliteCompiler
        .compile(
            Statement::Select(select_with_predicate(table.clone(), predicate).unwrap()),
            &SqliteCompiler.support(),
        )
        .unwrap();
    assert!(compiled.sql().contains("zeroship_json_equal("));

    let predicate = ResolvedPredicate::Membership {
        lhs: ResolvedOperand::Column(document),
        op: MembershipOp::NotIn,
        values: vec![Value::Json(r#"{"a":1,"b":2}"#.into())],
    };
    let compiled = SqliteCompiler
        .compile(
            Statement::Select(select_with_predicate(table, predicate).unwrap()),
            &SqliteCompiler.support(),
        )
        .unwrap();
    assert!(compiled.sql().contains("NOT zeroship_json_equal("));
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
                identity: table.column("id").unwrap(),
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
    assert!(postgres
        .sql()
        .contains("ORDER BY \"_distance_m\", \"source\".\"id\""));

    let sqlite = SqliteCompiler
        .compile(statement(), &SqliteCompiler.support())
        .unwrap();
    assert!(!sqlite.sql().contains("ST_DWithin"));
    assert!(!sqlite.sql().contains("LIMIT"));
    assert!(sqlite.sql().starts_with("SELECT \"source\".\"id\""));
}

#[test]
fn spatial_bind_preflight_follows_the_registered_compiler() {
    let statement = || {
        let table = search_table();
        Statement::SpatialNear(
            SpatialNearStatement::new(SpatialNearParts {
                projection: vec![ReturnedColumn {
                    column: table.column("id").unwrap(),
                    alias: None,
                }],
                identity: table.column("id").unwrap(),
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

    let mut sqlite_support = SqliteCompiler.support();
    sqlite_support.max_bind_parameters = 0;
    assert!(SqliteCompiler.compile(statement(), &sqlite_support).is_ok());
    let sqlite_registration = SqlRegistration::new(
        "sqlite-spatial-bind-accounting",
        SqlFamily::new("example.sqlite-spatial-bind-accounting"),
        SqliteCompiler,
        DownstreamCodecs,
        sqlite_support,
    )
    .unwrap();
    assert!(sqlite_registration.compile(statement()).is_ok());

    let mut postgres_support = PostgresCompiler.support();
    postgres_support.max_bind_parameters = 2;
    assert!(matches!(
        PostgresCompiler.compile(statement(), &postgres_support),
        Err(CompileError::BindLimitExceeded { limit: 2 })
    ));
}

#[test]
fn search_statements_require_a_positive_limit() {
    let table = search_table();
    assert!(VectorSearchStatement::new(VectorSearchParts {
        projection: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: None,
        }],
        identity: table.column("id").unwrap(),
        vector: table.column("embedding").unwrap(),
        query: Value::Bytes(vec![0; 8]),
        metric: zeroship_data_orm::sql::descriptors::VectorMetric::Cosine,
        predicate: ResolvedPredicate::Const(true),
        limit: 0,
        table: table.clone(),
    })
    .is_err());
    assert!(SpatialNearStatement::new(SpatialNearParts {
        projection: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: None,
        }],
        identity: table.column("id").unwrap(),
        spatial: table.column("location").unwrap(),
        point: zeroship_data_orm::value!({"lat":51.5,"lng":-0.1}),
        radius_m: 1000.0,
        predicate: ResolvedPredicate::Const(true),
        limit: 0,
        table,
    })
    .is_err());
}

#[test]
fn search_projection_cannot_shadow_its_distance_output() {
    let table = search_table();
    assert!(VectorSearchStatement::new(VectorSearchParts {
        projection: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: Some(Ident::parse_as("_distance", IdentRole::Alias).unwrap()),
        }],
        identity: table.column("id").unwrap(),
        vector: table.column("embedding").unwrap(),
        query: Value::Bytes(vec![0; 8]),
        metric: zeroship_data_orm::sql::descriptors::VectorMetric::Cosine,
        predicate: ResolvedPredicate::Const(true),
        limit: 1,
        table: table.clone(),
    })
    .is_err());
    assert!(SpatialNearStatement::new(SpatialNearParts {
        projection: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: Some(Ident::parse_as("_distance_m", IdentRole::Alias).unwrap()),
        }],
        identity: table.column("id").unwrap(),
        spatial: table.column("location").unwrap(),
        point: zeroship_data_orm::value!({"lat":51.5,"lng":-0.1}),
        radius_m: 1000.0,
        predicate: ResolvedPredicate::Const(true),
        limit: 1,
        table,
    })
    .is_err());
}

#[test]
fn search_tie_breakers_require_the_collection_identity() {
    let table = search_table();
    assert!(VectorSearchStatement::new(VectorSearchParts {
        projection: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: None,
        }],
        identity: table.column("label").unwrap(),
        vector: table.column("embedding").unwrap(),
        query: Value::Bytes(vec![0; 8]),
        metric: zeroship_data_orm::sql::descriptors::VectorMetric::Cosine,
        predicate: ResolvedPredicate::Const(true),
        limit: 1,
        table: table.clone(),
    })
    .is_err());
    assert!(SpatialNearStatement::new(SpatialNearParts {
        projection: vec![ReturnedColumn {
            column: table.column("id").unwrap(),
            alias: None,
        }],
        identity: table.column("label").unwrap(),
        spatial: table.column("location").unwrap(),
        point: zeroship_data_orm::value!({"lat":51.5,"lng":-0.1}),
        radius_m: 1000.0,
        predicate: ResolvedPredicate::Const(true),
        limit: 1,
        table,
    })
    .is_err());
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
                lock: zeroship_data_orm::sql::statement::RowLock::None,
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
        lock: zeroship_data_orm::sql::statement::RowLock::None,
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
        lock: zeroship_data_orm::sql::statement::RowLock::None,
    });
    assert!(statement.is_err());
}

#[test]
fn select_rejects_aggregate_shapes_without_portable_sql() {
    let source = search_table();
    let statement = |expression| {
        SelectStatement::new(SelectParts {
            table: source.clone(),
            joins: Vec::new(),
            projection: vec![SelectedExpression {
                expression,
                alias: Ident::parse_as("result", IdentRole::Alias).unwrap(),
            }],
            predicate: ResolvedPredicate::Const(true),
            group_by: Vec::new(),
            having: ResolvedPredicate::Const(true),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            distinct: false,
            lock: zeroship_data_orm::sql::statement::RowLock::None,
        })
    };
    assert!(statement(ResolvedOperand::Aggregate {
        function: zeroship_data_orm::sql::AggregateFunc::Count,
        column: None,
        distinct: true,
    })
    .is_err());
    assert!(statement(ResolvedOperand::Aggregate {
        function: zeroship_data_orm::sql::AggregateFunc::Sum,
        column: Some(source.column("label").unwrap()),
        distinct: false,
    })
    .is_err());
    assert!(statement(ResolvedOperand::Aggregate {
        function: zeroship_data_orm::sql::AggregateFunc::Sum,
        column: Some(source.column("amount").unwrap()),
        distinct: false,
    })
    .is_err());
}

#[test]
fn an_ungrouped_select_rejects_a_plain_having_clause() {
    let source = search_table();
    let statement = SelectStatement::new(SelectParts {
        table: source.clone(),
        joins: Vec::new(),
        projection: vec![SelectedExpression {
            expression: ResolvedOperand::Column(source.column("id").unwrap()),
            alias: Ident::parse_as("id", IdentRole::Alias).unwrap(),
        }],
        predicate: ResolvedPredicate::Const(true),
        group_by: Vec::new(),
        having: ResolvedPredicate::Compare {
            lhs: ResolvedOperand::Column(source.column("label").unwrap()),
            op: CompareOp::Eq,
            rhs: ResolvedPredicateValue::Bind {
                storage: StorageType::Text,
                value: Value::from("open"),
            },
        },
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: zeroship_data_orm::sql::statement::RowLock::None,
    });
    assert!(statement.is_err());
}

#[test]
fn select_revalidates_output_identifiers_for_the_alias_role() {
    let source = search_table();
    let statement = SelectStatement::new(SelectParts {
        table: source.clone(),
        joins: Vec::new(),
        projection: vec![SelectedExpression {
            expression: ResolvedOperand::Column(source.column("id").unwrap()),
            alias: Ident::parse_as("__zs_private", IdentRole::StoredColumn).unwrap(),
        }],
        predicate: ResolvedPredicate::Const(true),
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: zeroship_data_orm::sql::statement::RowLock::None,
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

#[test]
fn integer_storage_rejects_values_outside_the_portable_database_range() {
    let table = table();
    for value in [Value::from(u64::MAX), Value::Timestamp(1)] {
        let mut input = insert_parts(&table);
        input.rows[0][1] = Expression::Bind(value);
        assert_eq!(
            Insert::new(input).unwrap_err(),
            CompileError::InvalidStatement(
                "bound value does not match its physical storage type".into()
            )
        );
    }
}

#[test]
fn numeric_storage_rejects_lossy_integer_arithmetic_and_invalid_decimals() {
    let table = Table::new(
        SchemaName::new("app-numeric").unwrap(),
        Ident::parse_as("accounts", IdentRole::Collection).unwrap(),
        [
            ("id", StorageType::Integer),
            ("balance", StorageType::Integer),
            ("ratio", StorageType::Real),
            ("amount", StorageType::Decimal),
        ]
        .map(|(name, storage)| {
            (
                Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                storage,
            )
        }),
    )
    .unwrap();
    let balance = table.column("balance").unwrap();
    assert!(Update::new(UpdateParts {
        table: table.clone(),
        assignments: vec![Assignment {
            column: balance.clone(),
            value: Expression::Arithmetic {
                column: balance,
                operator: ArithmeticOperator::Add,
                operand: zeroship_data_orm::value!(1.5),
            },
        }],
        predicate: ResolvedPredicate::Const(true),
        scope: MutationScope::Matching,
        returning: Vec::new(),
    })
    .is_err());

    for column in ["ratio", "amount"] {
        let column = table.column(column).unwrap();
        assert!(Insert::new(InsertParts {
            table: table.clone(),
            columns: vec![column],
            rows: vec![vec![Expression::Bind(Value::Decimal(
                "not-a-decimal".into(),
            ))]],
            returning: Vec::new(),
            insert_generated_identity: false,
        })
        .is_err());
    }
}

#[test]
fn returning_projection_rejects_duplicate_output_names() {
    let table = table();
    let id = table.column("id").unwrap();
    let revision = table.column("revision").unwrap();
    for aliases in [(Some("result"), Some("result")), (None, Some("id"))] {
        let mut input = insert_parts(&table);
        input.returning = vec![
            ReturnedColumn {
                column: id.clone(),
                alias: aliases
                    .0
                    .map(|name| Ident::parse_as(name, IdentRole::Alias).unwrap()),
            },
            ReturnedColumn {
                column: revision.clone(),
                alias: aliases
                    .1
                    .map(|name| Ident::parse_as(name, IdentRole::Alias).unwrap()),
            },
        ];
        assert!(Insert::new(input).is_err());
    }
}

#[test]
fn array_mutations_accept_structured_json_from_storage_codecs() {
    let table = table();
    let document = table.column("document").unwrap();
    let statement = Update::new(UpdateParts {
        table,
        assignments: vec![Assignment {
            column: document.clone(),
            value: Expression::ArrayMutation {
                column: document,
                operator: ArrayOperator::Push,
                operand: zeroship_data_orm::value!({"nested":true}),
            },
        }],
        predicate: ResolvedPredicate::Const(true),
        scope: MutationScope::Matching,
        returning: Vec::new(),
    });
    assert!(statement.is_ok());
}

#[test]
fn first_row_mutations_require_the_collection_identity() {
    let table = table();
    let revision = table.column("revision").unwrap();
    let error = Update::new(UpdateParts {
        table: table.clone(),
        assignments: vec![Assignment {
            column: revision.clone(),
            value: Expression::Bind(Value::from(2)),
        }],
        predicate: ResolvedPredicate::Const(true),
        scope: MutationScope::First { target: revision },
        returning: Vec::new(),
    })
    .unwrap_err();
    assert_eq!(
        error,
        CompileError::InvalidStatement("first-row mutation requires the id column".into())
    );

    let revision = table.column("revision").unwrap();
    let error = Delete::new(DeleteParts {
        table,
        predicate: ResolvedPredicate::Const(true),
        scope: MutationScope::First { target: revision },
        returning: Vec::new(),
    })
    .unwrap_err();
    assert_eq!(
        error,
        CompileError::InvalidStatement("first-row mutation requires the id column".into())
    );
}

#[test]
fn first_row_mutations_choose_the_lowest_identity() {
    for compiler in [&PostgresCompiler as &dyn SqlCompiler, &SqliteCompiler] {
        let table = table();
        let id = table.column("id").unwrap();
        let revision = table.column("revision").unwrap();
        let update = Statement::Update(
            Update::new(UpdateParts {
                table: table.clone(),
                assignments: vec![Assignment {
                    column: revision,
                    value: Expression::Bind(Value::from(2)),
                }],
                predicate: ResolvedPredicate::Const(true),
                scope: MutationScope::First { target: id.clone() },
                returning: Vec::new(),
            })
            .unwrap(),
        );
        let delete = Statement::Delete(
            Delete::new(DeleteParts {
                table,
                predicate: ResolvedPredicate::Const(true),
                scope: MutationScope::First { target: id },
                returning: Vec::new(),
            })
            .unwrap(),
        );

        for statement in [update, delete] {
            let query = compiler.compile(statement, &compiler.support()).unwrap();
            assert!(
                query.sql().contains(" ORDER BY \"id\" LIMIT 1"),
                "{}",
                query.sql()
            );
        }
    }
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
fn upsert_normalizes_record_derived_assignments() {
    fn assignment(table: &Table, name: &str, value: Value) -> Assignment {
        Assignment {
            column: table.column(name).unwrap(),
            value: Expression::Bind(value),
        }
    }

    let table = table();
    let mut first = parts(&table);
    first.update = vec![
        assignment(&table, "revision", Value::from(9)),
        assignment(&table, "payload", Value::Bytes(vec![1, 2, 3])),
    ];
    let mut second = parts(&table);
    second.update = vec![
        assignment(&table, "payload", Value::Bytes(vec![1, 2, 3])),
        assignment(&table, "revision", Value::from(9)),
    ];

    let first = SqlRegistration::postgres()
        .compile(Statement::Upsert(Upsert::new(first).unwrap()))
        .unwrap();
    let second = SqlRegistration::postgres()
        .compile(Statement::Upsert(Upsert::new(second).unwrap()))
        .unwrap();

    assert_eq!(first.sql(), second.sql());
    assert_eq!(first.params(), second.params());
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
struct PermissiveCompiler(SqlSupport);

impl SqlCompiler for PermissiveCompiler {
    fn support(&self) -> SqlSupport {
        self.0
    }

    fn bind_parameters(&self, _: &Statement) -> usize {
        0
    }

    fn check(&self, _: &Requirements, _: &SqlSupport) -> Result<(), CompileError> {
        Ok(())
    }

    fn compile(&self, statement: Statement, _: &SqlSupport) -> Result<CompiledQuery, CompileError> {
        PostgresCompiler.compile(statement, &PostgresCompiler.support())
    }

    fn compile_identity_allocation(
        &self,
        request: IdentityRequest,
        _: &SqlSupport,
    ) -> Result<IdentityPlan, CompileError> {
        PostgresCompiler.compile_identity_allocation(request, &PostgresCompiler.support())
    }
}

#[derive(Clone, Copy)]
struct OverbindingCompiler(SqlSupport);

impl SqlCompiler for OverbindingCompiler {
    fn support(&self) -> SqlSupport {
        self.0
    }

    fn check(&self, _: &Requirements, _: &SqlSupport) -> Result<(), CompileError> {
        Ok(())
    }

    fn compile(&self, _: Statement, _: &SqlSupport) -> Result<CompiledQuery, CompileError> {
        Ok(CompiledQuery::new(
            "SELECT $1, $2".into(),
            vec![Value::from(1), Value::from(2)],
        ))
    }

    fn compile_identity_allocation(
        &self,
        _: IdentityRequest,
        _: &SqlSupport,
    ) -> Result<IdentityPlan, CompileError> {
        Ok(IdentityPlan {
            reservation: None,
            allocation: zeroship_data_orm::sql::compiler::IdentityReadPlan::Rows(
                CompiledQuery::new("SELECT $1, $2".into(), vec![Value::from(1), Value::from(2)]),
            ),
        })
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
        SqlFamily::new("example.test-sql"),
        DownstreamCompiler,
        DownstreamCodecs,
        DownstreamCompiler.support(),
    )
    .unwrap();
    assert_ne!(
        registration.identity(),
        SqlRegistration::postgres().identity()
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
fn registration_enforces_common_support_even_when_a_compiler_check_is_permissive() {
    let mut limited = PostgresCompiler.support();
    limited.returning = false;
    let compiler = PermissiveCompiler(limited);

    let registration = SqlRegistration::new(
        "limited-sql",
        SqlFamily::new("example.limited-sql"),
        compiler,
        DownstreamCodecs,
        limited,
    )
    .unwrap();
    assert!(matches!(
        registration.compile(Statement::Insert(
            Insert::new(insert_parts(&table())).unwrap()
        )),
        Err(CompileError::Unsupported("returning projections"))
    ));

    let mut overclaimed = limited;
    overclaimed.returning = true;
    assert!(SqlRegistration::new(
        "overclaimed-sql",
        SqlFamily::new("example.overclaimed-sql"),
        compiler,
        DownstreamCodecs,
        overclaimed,
    )
    .is_err());
}

#[test]
fn registration_enforces_bind_limits_on_downstream_compiler_output() {
    let mut support = PostgresCompiler.support();
    support.max_bind_parameters = 1;
    let registration = SqlRegistration::new(
        "overbinding-sql",
        SqlFamily::new("example.overbinding-sql"),
        OverbindingCompiler(support),
        DownstreamCodecs,
        support,
    )
    .unwrap();
    let source = search_table();
    let statement = Statement::Select(
        SelectStatement::new(SelectParts {
            table: source.clone(),
            joins: Vec::new(),
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
            lock: zeroship_data_orm::sql::statement::RowLock::None,
        })
        .unwrap(),
    );
    assert!(matches!(
        registration.compile(statement),
        Err(CompileError::BindLimitExceeded { limit: 1 })
    ));

    let table = table();
    let request = IdentityRequest::new(table.clone(), table.column("id").unwrap(), 1).unwrap();
    assert!(matches!(
        registration.compile_identity_allocation(request),
        Err(CompileError::BindLimitExceeded { limit: 1 })
    ));
}

#[test]
fn identity_allocation_sql_belongs_to_the_registered_compiler() {
    use zeroship_data_orm::sql::{compiler::IdentityReadPlan, statement::IdentityRequest};

    let table = table();
    let request = || IdentityRequest::new(table.clone(), table.column("id").unwrap(), 2).unwrap();
    let postgres = SqlRegistration::postgres()
        .compile_identity_allocation(request())
        .unwrap();
    assert!(postgres.reservation.is_none());
    assert!(matches!(postgres.allocation, IdentityReadPlan::Rows(_)));

    let sqlite = SqlRegistration::sqlite()
        .compile_identity_allocation(request())
        .unwrap();
    assert!(sqlite.reservation.is_some());
    assert!(matches!(
        sqlite.allocation,
        IdentityReadPlan::MaximumAndCounter { .. }
    ));
}
