use zeroship_data_orm::{
    sql::{
        CompareOp, Ident, IdentRole, SchemaName,
        compiler::{CompileError, PostgresCompiler, Requirements, SqlCompiler, SqliteCompiler},
        statement::{
            Assignment, Comparison, Expression, ReturnedColumn, Statement, StorageType, Table,
            Upsert, UpsertParts,
        },
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
        assert!(
            query
                .sql()
                .starts_with("INSERT INTO \"app-upserts\".\"entries\"")
        );
        assert!(!query.sql().contains("DROP TABLE"));
        assert!(!format!("{query:?}").contains("DROP TABLE"));
    }
}
