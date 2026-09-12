use zeroship_data_orm::sql::{
    statement::{
        ResolvedOperand, ResolvedPredicate, RowLock, SelectParts, SelectStatement,
        SelectedExpression, StorageType, Table,
    },
    CompareOp, Ident, IdentRole, Literal, Operand, Predicate, PredicateError, SchemaName,
    MAX_PREDICATE_DEPTH,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).unwrap()
}

fn input_leaf() -> Predicate {
    Predicate::compare(
        Operand::column(column("age")),
        CompareOp::Eq,
        Operand::Lit(Literal::Int(1)),
    )
}

fn input_spine(depth: usize) -> Predicate {
    if depth == 1 {
        return input_leaf();
    }
    Predicate::Not(Box::new(input_spine(depth - 1)))
}

fn resolved_spine(depth: usize) -> ResolvedPredicate {
    if depth == 1 {
        return ResolvedPredicate::Const(true);
    }
    ResolvedPredicate::Not(Box::new(resolved_spine(depth - 1)))
}

#[test]
fn smart_constructors_refuse_excessive_predicate_depth() {
    let accepted = input_spine(MAX_PREDICATE_DEPTH - 1);
    assert!(Predicate::and(vec![accepted]).is_ok());

    let refused = input_spine(MAX_PREDICATE_DEPTH);
    assert_eq!(
        Predicate::and(vec![refused]).unwrap_err(),
        PredicateError::TooDeep {
            depth: MAX_PREDICATE_DEPTH + 1,
        }
    );
}

#[test]
fn resolved_statement_constructors_recheck_predicate_depth() {
    let table = Table::aliased(
        SchemaName::new("app").unwrap(),
        Ident::parse_as("records", IdentRole::Collection).unwrap(),
        Ident::parse_as("source", IdentRole::Alias).unwrap(),
        [(column("id"), StorageType::Text)],
    )
    .unwrap();
    let id = table.column("id").unwrap();
    let build = |predicate| {
        SelectStatement::new(SelectParts {
            table: table.clone(),
            joins: Vec::new(),
            projection: vec![SelectedExpression {
                expression: ResolvedOperand::Column(id.clone()),
                alias: Ident::parse_as("id", IdentRole::Alias).unwrap(),
            }],
            predicate,
            group_by: Vec::new(),
            having: ResolvedPredicate::Const(true),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            distinct: false,
            lock: RowLock::None,
        })
    };

    assert!(build(resolved_spine(MAX_PREDICATE_DEPTH)).is_ok());
    let error = build(resolved_spine(MAX_PREDICATE_DEPTH + 1)).unwrap_err();
    assert!(error.to_string().contains("complexity budget"));
}
