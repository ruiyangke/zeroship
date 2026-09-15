use super::*;
use crate::sql::{
    Ident, IdentRole, SchemaName,
    statement::{
        Assignment, Expression, MutationScope, ResolvedPredicate, Statement, StorageType, Table,
        Update, UpdateParts, Upsert, UpsertParts,
    },
};

fn timestamp_statement(upsert: bool, offset_micros: i64) -> Result<Statement, CompileError> {
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
        value: Expression::DatabaseTimestamp { offset_micros },
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
            for offset in [0, 1_000, -1_000, 1_234_000, -1_234_000] {
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

// The bound offset carries the sign and every microsecond of the requested
// shift. PostgreSQL binds exact decimal interval text and never a float, so the
// span edges keep their last digit. SQLite binds the integer millisecond count.
#[test]
fn database_timestamp_offsets_bind_their_signed_microsecond_value() {
    use crate::value::Value;
    for (offset, interval) in [
        (-1_000, "-0.001000 seconds"),
        (1_000, "0.001000 seconds"),
        (-1_234_000, "-1.234000 seconds"),
        (1_234_000, "1.234000 seconds"),
        (-500_000, "-0.500000 seconds"),
    ] {
        for upsert in [false, true] {
            // An upsert binds its inserted identity before the conflict update.
            let identity = upsert.then(|| crate::value!(1));
            let query = PostgresCompiler
                .compile(
                    timestamp_statement(upsert, offset).unwrap(),
                    &PostgresCompiler.support(),
                )
                .unwrap();
            let expected: Vec<Value> = identity
                .iter()
                .cloned()
                .chain([Value::from(interval)])
                .collect();
            assert_eq!(
                query.params(),
                expected.as_slice(),
                "offset {offset}, upsert {upsert}"
            );
            let sqlite = SqliteCompiler
                .compile(
                    timestamp_statement(upsert, offset).unwrap(),
                    &SqliteCompiler.support(),
                )
                .unwrap();
            let expected: Vec<Value> = identity
                .iter()
                .cloned()
                .chain([Value::from(offset / 1_000)])
                .collect();
            assert_eq!(sqlite.params(), expected.as_slice(), "offset {offset}");
        }
    }
    // The span edges exceed the range where a double holds every integer, so
    // the bound text is read back as an exact decimal and compared with the
    // offset itself. A renderer that went through a float loses the last digit
    // here rather than anywhere else.
    let span =
        crate::sql::temporal::MAX_TIMESTAMP_MICROS - crate::sql::temporal::MIN_TIMESTAMP_MICROS;
    for offset in [span, -span, span - 1, 1 - span] {
        let query = PostgresCompiler
            .compile(
                timestamp_statement(false, offset).unwrap(),
                &PostgresCompiler.support(),
            )
            .unwrap();
        let [Value::String(text)] = query.params() else {
            panic!("one bound interval")
        };
        let digits = text
            .strip_suffix(" seconds")
            .expect("interval text names its unit");
        let (whole, fraction) = digits.split_once('.').expect("exact decimal seconds");
        assert_eq!(fraction.len(), 6, "{text}");
        let magnitude = whole.trim_start_matches('-').parse::<i64>().unwrap() * 1_000_000
            + fraction.parse::<i64>().unwrap();
        let signed = if digits.starts_with('-') {
            -magnitude
        } else {
            magnitude
        };
        assert_eq!(signed, offset, "{text}");
    }
    // Rejection control: SQLite stores whole milliseconds, so an offset with a
    // finer part is refused there while PostgreSQL renders it exactly.
    for offset in [1_i64, -1, 999, 1_001] {
        assert_eq!(
            SqliteCompiler.compile(
                timestamp_statement(false, offset).unwrap(),
                &SqliteCompiler.support()
            ),
            Err(CompileError::TimestampPrecisionUnsupported),
            "{offset}"
        );
        let query = PostgresCompiler
            .compile(
                timestamp_statement(false, offset).unwrap(),
                &PostgresCompiler.support(),
            )
            .unwrap();
        let fraction = (offset % 1_000_000).unsigned_abs();
        let sign = if offset < 0 { "-" } else { "" };
        assert_eq!(
            query.params(),
            [Value::from(format!("{sign}0.{fraction:06} seconds"))].as_slice(),
            "{offset}"
        );
    }
}

#[test]
fn database_timestamp_ast_enforces_the_native_offset_domain() {
    let span =
        crate::sql::temporal::MAX_TIMESTAMP_MICROS - crate::sql::temporal::MIN_TIMESTAMP_MICROS;
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
