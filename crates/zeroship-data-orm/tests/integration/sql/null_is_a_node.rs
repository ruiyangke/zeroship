use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::sql::{
    compiler::CompileError,
    statement::{Expression, Insert, InsertParts, StorageType, Table},
    Ident, IdentRole, Literal, LiteralError, LiteralSet, MembershipOp, Operand, Predicate,
};
use zeroship_data_orm::value;
use zeroship_data_orm::Value;

#[test]
fn filter_text_uses_the_validated_literal_boundary() {
    let error = zeroship_data_orm::sql::filter::decode(&value!({"name":"bad\0value"}))
        .expect_err("NUL text must be refused before compilation");
    assert!(error.to_string().contains("NUL"));
}

fn column(name: &str) -> Operand {
    Operand::column(Ident::parse_as(name, IdentRole::Column).unwrap())
}

#[test]
fn null_has_no_literal_representation() {
    for literal in [
        Literal::Bool(true),
        Literal::Int(1),
        Literal::float(1.5).unwrap(),
        Literal::text("value").unwrap(),
        Literal::Bytes(vec![0]),
    ] {
        assert_ne!(literal.type_name(), "null");
    }
    assert_eq!(Literal::from_optional(None), None);
}

#[test]
fn sql_null_cannot_hide_inside_a_bound_value() {
    let table = Table::new(
        SchemaName::new("app").unwrap(),
        Ident::parse_as("entries", IdentRole::Collection).unwrap(),
        [(
            Ident::parse_as("title", IdentRole::Column).unwrap(),
            StorageType::Text,
        )],
    )
    .unwrap();
    let title = table.column("title").unwrap();
    let error = Insert::new(InsertParts {
        table,
        columns: vec![title],
        rows: vec![vec![Expression::Bind(Value::Null)]],
        returning: Vec::new(),
        insert_generated_identity: false,
    })
    .unwrap_err();
    assert_eq!(
        error,
        CompileError::InvalidStatement("SQL null must use the explicit null expression".into())
    );
}

#[test]
fn membership_rewrites_nulls_into_predicate_nodes() {
    let operand = column("status");
    let in_null = Predicate::membership(operand.clone(), MembershipOp::In, vec![None]).unwrap();
    assert!(matches!(in_null, Predicate::IsNull { negated: false, .. }));

    let not_in_null =
        Predicate::membership(operand.clone(), MembershipOp::NotIn, vec![None]).unwrap();
    assert!(matches!(
        not_in_null,
        Predicate::IsNull { negated: true, .. }
    ));

    let mixed = Predicate::membership(
        operand.clone(),
        MembershipOp::In,
        vec![Some(Literal::Int(1)), None],
    )
    .unwrap();
    assert!(matches!(mixed, Predicate::Or(children) if
        children.iter().any(|child| matches!(child, Predicate::Membership { .. }))
        && children.iter().any(|child| matches!(child, Predicate::IsNull { negated: false, .. }))
    ));

    let mixed = Predicate::membership(
        operand.clone(),
        MembershipOp::NotIn,
        vec![Some(Literal::Int(1)), None],
    )
    .unwrap();
    assert!(matches!(mixed, Predicate::And(children) if
        children.iter().any(|child| matches!(child, Predicate::Membership { .. }))
        && children.iter().any(|child| matches!(child, Predicate::IsNull { negated: true, .. }))
    ));

    assert_eq!(
        Predicate::membership(operand.clone(), MembershipOp::In, vec![]).unwrap(),
        Predicate::never()
    );
    assert_eq!(
        Predicate::membership(operand, MembershipOp::NotIn, vec![]).unwrap(),
        Predicate::always()
    );
}

#[test]
fn literal_sets_enforce_shape_and_budget() {
    assert_eq!(LiteralSet::new(vec![]), Err(LiteralError::EmptyLiteralSet));
    assert_eq!(
        LiteralSet::new(vec![Literal::Int(1), Literal::text("two").unwrap()]),
        Err(LiteralError::HeterogeneousSet {
            expected: "int",
            found: "text",
        })
    );

    let limit = zeroship_data_orm::sql::MAX_MEMBERSHIP_LIST_LEN;
    let accepted = (0..limit).map(|value| Literal::Int(value as i64)).collect();
    let refused = (0..=limit)
        .map(|value| Literal::Int(value as i64))
        .collect();
    assert!(LiteralSet::new(accepted).is_ok());
    assert_eq!(
        LiteralSet::new(refused),
        Err(LiteralError::MembershipListTooLong { len: limit + 1 })
    );
}
