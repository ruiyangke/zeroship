use super::*;
use crate::sql::statement::{
    ResolvedJoin, ResolvedOperand, ResolvedPredicate, ResolvedPredicateValue, RowLock,
    SelectParts, SelectStatement, SelectSummary, SelectedExpression, Statement, StorageType,
    Table,
};
use crate::sql::{CompareOp, Ident, IdentRole, JoinKind, SchemaName};

fn alias(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Alias).unwrap()
}

fn table(name: &str, source: &str) -> Table {
    Table::aliased(
        SchemaName::new("locks").unwrap(),
        Ident::parse_as(name, IdentRole::Collection).unwrap(),
        alias(source),
        [(
            Ident::parse_as("id", IdentRole::Column).unwrap(),
            StorageType::Text,
        )],
    )
    .unwrap()
}

fn parts(join: Option<JoinKind>, lock: RowLock) -> SelectParts {
    let sessions = table("sessions", "s");
    let mut joins = Vec::new();
    if let Some(kind) = join {
        let grants = table("grants", "g");
        joins.push(ResolvedJoin {
            kind,
            on: ResolvedPredicate::Compare {
                lhs: ResolvedOperand::Column(grants.column("id").unwrap()),
                op: CompareOp::Eq,
                rhs: ResolvedPredicateValue::Operand(ResolvedOperand::Column(
                    sessions.column("id").unwrap(),
                )),
            },
            table: grants,
        });
    }
    SelectParts {
        projection: vec![SelectedExpression {
            expression: ResolvedOperand::Column(sessions.column("id").unwrap()),
            alias: alias("v0"),
        }],
        table: sessions,
        joins,
        predicate: ResolvedPredicate::Const(true),
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: Some(5),
        offset: Some(10),
        distinct: false,
        lock,
    }
}

fn required(of: &[&str]) -> RowLock {
    RowLock::Required {
        of: of.iter().map(|name| alias(name)).collect(),
    }
}

#[test]
fn postgres_row_locks_follow_pagination_and_name_only_the_locked_aliases() {
    let statement = Statement::select(
        SelectStatement::new(parts(Some(JoinKind::Inner), required(&["s"]))).unwrap(),
    );
    let requirements = Requirements::for_statement(&statement);
    assert!(requirements.row_locks);
    let query = PostgresCompiler
        .compile(statement, &PostgresCompiler.support())
        .unwrap();
    assert!(
        query.sql().ends_with(" LIMIT $1 OFFSET $2 FOR UPDATE OF \"s\""),
        "{}",
        query.sql()
    );
    assert_eq!(query.params().len(), requirements.bind_parameters);

    let unqualified = PostgresCompiler
        .compile(
            Statement::select(SelectStatement::new(parts(None, required(&[]))).unwrap()),
            &PostgresCompiler.support(),
        )
        .unwrap();
    assert!(unqualified.sql().ends_with(" OFFSET $2 FOR UPDATE"), "{}", unqualified.sql());

    let several = PostgresCompiler
        .compile(
            Statement::select(
                SelectStatement::new(parts(Some(JoinKind::Inner), required(&["s", "g"])))
                    .unwrap(),
            ),
            &PostgresCompiler.support(),
        )
        .unwrap();
    assert!(several.sql().ends_with(" FOR UPDATE OF \"s\", \"g\""), "{}", several.sql());

    // Control: a read without a lock renders no locking clause.
    let plain = PostgresCompiler
        .compile(
            Statement::select(SelectStatement::new(parts(None, RowLock::None)).unwrap()),
            &PostgresCompiler.support(),
        )
        .unwrap();
    assert!(!plain.sql().contains("FOR UPDATE"), "{}", plain.sql());
}

#[test]
fn required_row_locks_are_refused_where_the_backend_has_none() {
    let statement =
        || Statement::select(SelectStatement::new(parts(None, required(&[]))).unwrap());
    assert_eq!(
        SqliteCompiler
            .compile(statement(), &SqliteCompiler.support())
            .unwrap_err(),
        CompileError::Unsupported("row locks")
    );
    let mut without_row_locks = PostgresCompiler.support();
    without_row_locks.row_locks = false;
    assert_eq!(
        PostgresCompiler
            .compile(statement(), &without_row_locks)
            .unwrap_err(),
        CompileError::Unsupported("row locks")
    );
    // Control: the internal write-target probe compiles on both backends,
    // with no locking clause where the single writer serializes writes.
    let probe =
        || Statement::select(SelectStatement::new(parts(None, RowLock::WriteTargets)).unwrap());
    let sqlite = SqliteCompiler
        .compile(probe(), &SqliteCompiler.support())
        .unwrap();
    assert!(!sqlite.sql().contains("FOR UPDATE"), "{}", sqlite.sql());
    assert!(!Requirements::for_statement(&probe()).row_locks);
    let postgres = PostgresCompiler
        .compile(probe(), &PostgresCompiler.support())
        .unwrap();
    assert!(postgres.sql().ends_with(" FOR UPDATE"), "{}", postgres.sql());
}

#[test]
fn row_lock_targets_must_be_distinct_non_nullable_sources() {
    for (join, lock) in [
        (Some(JoinKind::Left), required(&[])),
        (Some(JoinKind::Left), required(&["g"])),
        (Some(JoinKind::Inner), required(&["missing"])),
        (Some(JoinKind::Inner), required(&["s", "s"])),
    ] {
        assert!(
            SelectStatement::new(parts(join, lock.clone())).is_err(),
            "{join:?} {lock:?}"
        );
    }
    // Controls: the non-nullable side of a left join, and both inner sources.
    assert!(SelectStatement::new(parts(Some(JoinKind::Left), required(&["s"]))).is_ok());
    assert!(SelectStatement::new(parts(Some(JoinKind::Inner), required(&["g", "s"]))).is_ok());
    assert!(SelectStatement::new(parts(Some(JoinKind::Inner), required(&[]))).is_ok());
}

#[test]
fn row_locks_refuse_grouped_distinct_and_summarized_selects() {
    for lock in [required(&[]), RowLock::WriteTargets] {
        let mut distinct = parts(None, lock.clone());
        distinct.distinct = true;
        assert!(SelectStatement::new(distinct).is_err(), "{lock:?}");
        let mut grouped = parts(None, lock.clone());
        grouped.projection = vec![SelectedExpression {
            expression: ResolvedOperand::Aggregate {
                function: crate::sql::AggregateFunc::Count,
                column: None,
                distinct: false,
            },
            alias: alias("v0"),
        }];
        assert!(SelectStatement::new(grouped).is_err(), "{lock:?}");
        for summary in [SelectSummary::Count, SelectSummary::Exists] {
            let summarized = SelectStatement::new(parts(None, lock.clone()))
                .unwrap()
                .summarize(summary);
            assert!(summarized.validate().is_err(), "{lock:?} {summary:?}");
        }
    }
    // Control: an unlocked summary stays valid.
    let summarized = SelectStatement::new(parts(None, RowLock::None))
        .unwrap()
        .summarize(SelectSummary::Count);
    assert!(summarized.validate().is_ok());
}
