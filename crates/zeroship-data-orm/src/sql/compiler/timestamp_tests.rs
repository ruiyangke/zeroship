use super::*;
use crate::sql::{
    Ident, IdentRole, SchemaName,
    statement::{
        Assignment, Expression, MutationScope, ResolvedPredicate, Statement, StorageType, Table,
        Update, UpdateParts, Upsert, UpsertParts,
    },
};

fn timestamp_statement(upsert: bool, offset_millis: i64) -> Result<Statement, CompileError> {
    let table = Table::new(
        SchemaName::new("public").unwrap(),
        Ident::parse_as("moments", IdentRole::Collection).unwrap(),
        [
            ("id", StorageType::Integer),
            ("stamp", StorageType::Timestamp),
        ]
        .map(|(name, storage)| {
            (
                Ident::parse_as(name, IdentRole::StoredColumn).unwrap(),
                storage,
            )
        }),
    )
    .unwrap();
    let assignment = Assignment {
        column: table.column("stamp").unwrap(),
        value: Expression::DatabaseTimestamp { offset_millis },
    };
    if upsert {
        Upsert::new(UpsertParts {
            insert: vec![Assignment {
                column: table.column("id").unwrap(),
                value: Expression::Bind(crate::value!(1)),
            }],
            conflict: vec![table.column("id").unwrap()],
            update: vec![assignment],
            table,
            condition: None,
            returning: vec![],
            insert_generated_identity: false,
        })
        .map(Statement::Upsert)
    } else {
        Update::new(UpdateParts {
            table,
            assignments: vec![assignment],
            predicate: ResolvedPredicate::Const(true),
            scope: MutationScope::Matching,
            returning: vec![],
        })
        .map(Statement::Update)
    }
}

#[test]
fn database_timestamp_requirements_account_for_every_offset_parameter() {
    for compiler in [&PostgresCompiler as &dyn SqlCompiler, &SqliteCompiler] {
        for upsert in [false, true] {
            for offset in [0, 1, -1, 1_234, -1_234] {
                let statement = timestamp_statement(upsert, offset).unwrap();
                let requirements = Requirements::for_statement(&statement);
                let declared = compiler.bind_parameters(&statement);
                let query = compiler.compile(statement, &compiler.support()).unwrap();
                assert!(!query.params().is_empty());
                assert_eq!(requirements.bind_parameters, query.params().len());
                assert_eq!(declared, query.params().len());
                let mut limited = compiler.support();
                limited.max_bind_parameters = query.params().len() - 1;
                assert_eq!(
                    compiler.check(&requirements, &limited),
                    Err(CompileError::BindLimitExceeded {
                        limit: limited.max_bind_parameters,
                    })
                );
                assert!(matches!(
                    compiler.compile(timestamp_statement(upsert, offset).unwrap(), &limited),
                    Err(CompileError::BindLimitExceeded { .. })
                ));
            }
        }
    }
}

// The bound offset carries the sign and every millisecond of the requested
// shift. PostgreSQL binds interval text, SQLite binds the integer offset.
#[test]
fn database_timestamp_offsets_bind_their_signed_millisecond_value() {
    use crate::value::Value;
    for (offset, interval) in [
        (-1, "-0.001 seconds"),
        (1, "0.001 seconds"),
        (-1_234, "-1.234 seconds"),
        (1_234, "1.234 seconds"),
    ] {
        for upsert in [false, true] {
            // An upsert binds its inserted identity before the conflict update.
            let identity = upsert.then(|| crate::value!(1));
            for (compiler, bound_offset) in [
                (&PostgresCompiler as &dyn SqlCompiler, Value::from(interval)),
                (&SqliteCompiler, Value::from(offset)),
            ] {
                let query = compiler
                    .compile(
                        timestamp_statement(upsert, offset).unwrap(),
                        &compiler.support(),
                    )
                    .unwrap();
                let expected: Vec<Value> = identity.iter().cloned().chain([bound_offset]).collect();
                assert_eq!(
                    query.params(),
                    expected.as_slice(),
                    "offset {offset}, upsert {upsert}"
                );
            }
        }
    }
}

#[test]
fn database_timestamp_ast_enforces_the_native_offset_domain() {
    let span =
        crate::sql::temporal::MAX_TIMESTAMP_MILLIS - crate::sql::temporal::MIN_TIMESTAMP_MILLIS;
    for upsert in [false, true] {
        for offset in [-span, 0, span] {
            assert!(timestamp_statement(upsert, offset).is_ok());
        }
        for offset in [i64::MIN, -span - 1, span + 1, i64::MAX] {
            assert!(
                matches!(
                    timestamp_statement(upsert, offset),
                    Err(CompileError::InvalidStatement(_))
                ),
                "out-of-calendar offset accepted: {offset}, upsert: {upsert}"
            );
        }
    }
}
