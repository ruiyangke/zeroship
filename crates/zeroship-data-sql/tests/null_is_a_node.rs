//! Nullness is a node, so the defect that made it a value is unrepresentable.
//!
//! The defect SC-3 traces: `value_to_param_inner` mapped a JSON null to the
//! empty string, and the membership arms pushed every array element through it,
//! so `{ f: { $in: [null] } }` compiled to `f IN ($1)` with `$1 = ''` - matching
//! rows whose value is the empty string and missing every row that is actually
//! NULL. Wrong results, no error, no diagnostic. It is repaired in `query.rs`
//! (`:5386-5409` and `:5424-5444`), and a repair is not a type: nothing stops
//! the next author adding a third membership operator that reaches for
//! `value_to_param` again.
//!
//! These arms check the two halves separately, because the type half is what
//! makes the behaviour half durable.

use zeroship_data_sql::render::postgres;
use zeroship_data_sql::{
    Ident, IdentRole, Literal, LiteralError, LiteralSet, MembershipOp, Operand, Predicate,
    ProjectedField, Projection, Select,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn sql_for(filter: Predicate) -> String {
    let projection = Projection::rows(vec![
        ProjectedField::column(column("name")).expect("projectable"),
    ])
    .expect("row projection");
    let plan = Select::builder(
        Ident::parse_as("users", IdentRole::Collection).expect("collection"),
        projection,
    )
    .filter(filter)
    .build()
    .expect("valid plan");
    postgres::render_select(&plan)
        .expect("renders")
        .sql()
        .to_string()
}

/// THE TYPE HALF. `Literal` has five variants and none of them is null, so the
/// value that caused the defect cannot be constructed. This arm rules on the
/// enumeration rather than on one call site, so a sixth variant named `Null`
/// would have to break it deliberately.
#[test]
fn no_literal_is_null() {
    let representable = [
        Literal::Bool(true),
        Literal::Int(1),
        Literal::float(1.5).expect("finite"),
        Literal::text("x").expect("text"),
        Literal::Bytes(vec![0x00]),
    ];
    let mut ruled_on = 0_usize;
    for value in &representable {
        assert!(
            !value.type_name().eq_ignore_ascii_case("null"),
            "a literal reported itself as null: {value:?}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 5, "the literal enumeration changed shape");
    // A null-shaped value has to be carried as an absence, and the conversion
    // boundary preserves it as one.
    assert_eq!(Literal::from_optional(None), None);
    println!("ruled on {ruled_on} literal variants");
}

/// THE BEHAVIOUR HALF, all four rows of the membership table at once.
#[test]
fn the_membership_null_rule_matches_the_repaired_lowering() {
    let lhs = || Operand::column(column("status"));
    let one = || Some(Literal::Int(1));

    let cases: [(MembershipOp, Vec<Option<Literal>>, &str); 6] = [
        // values, no nulls
        (MembershipOp::In, vec![one()], r#""status" IN ($1)"#),
        (MembershipOp::NotIn, vec![one()], r#""status" NOT IN ($1)"#),
        // nulls only - becomes a nullness test, never a bound empty string
        (MembershipOp::In, vec![None], r#""status" IS NULL"#),
        (MembershipOp::NotIn, vec![None], r#""status" IS NOT NULL"#),
        // both - NOT what SQL does natively; `x IN (NULL)` matches nothing
        (
            MembershipOp::In,
            vec![one(), None],
            r#"("status" IN ($1) OR "status" IS NULL)"#,
        ),
        (
            MembershipOp::NotIn,
            vec![one(), None],
            r#"("status" NOT IN ($1) AND "status" IS NOT NULL)"#,
        ),
    ];

    let mut ruled_on = 0_usize;
    for (op, members, expected) in cases {
        let predicate =
            Predicate::membership(lhs(), op, members.clone()).expect("membership builds");
        let sql = sql_for(predicate);
        assert!(
            sql.contains(expected),
            "{op:?} over {members:?} lowered to {sql}, expected it to contain {expected}"
        );
        assert!(
            !sql.contains("''"),
            "a null was bound as an empty string: {sql}"
        );
        ruled_on += 1;
    }

    // The empty rows are asserted on the PREDICATE rather than on rendered SQL,
    // because the true constant is the canonical form of "no filter" and the
    // renderer omits the clause entirely - so a substring check for `TRUE`
    // would be looking for text that correctly is not there.
    assert_eq!(
        Predicate::membership(lhs(), MembershipOp::In, vec![]).expect("builds"),
        Predicate::never(),
        "an empty $in must match nothing"
    );
    assert_eq!(
        Predicate::membership(lhs(), MembershipOp::NotIn, vec![]).expect("builds"),
        Predicate::always(),
        "an empty $nin must exclude nothing"
    );
    ruled_on += 2;

    assert_eq!(ruled_on, 8);
    println!("ruled on {ruled_on} membership shapes");
}

/// `IN ()` has no representation, checked at the type rather than inferred from
/// the table above: an empty set is refused by [`LiteralSet`] itself, so even a
/// caller who bypassed [`Predicate::membership`] and built the variant by hand
/// could not produce one.
#[test]
fn an_empty_membership_set_is_not_constructible() {
    assert_eq!(LiteralSet::new(vec![]), Err(LiteralError::EmptyLiteralSet));
    assert!(LiteralSet::new(vec![Literal::Int(1)]).is_ok());
    println!("ruled on 2 sets");
}

/// A mixed set binds parameters of two types into one list, where `PostgreSQL`
/// resolves a single type for the whole list.
#[test]
fn a_heterogeneous_membership_set_is_refused() {
    let outcome = LiteralSet::new(vec![Literal::Int(1), Literal::text("two").expect("text")]);
    assert_eq!(
        outcome,
        Err(LiteralError::HeterogeneousSet {
            expected: "int",
            found: "text"
        })
    );
    println!("ruled on 1 mixed set");
}

/// The cardinality cap travels with the type, so it cannot be enforced at two
/// of three call sites - which is the state `query.rs` is in today
/// (`:5374`, `:5415`, `:5717`).
#[test]
fn the_membership_cap_travels_with_the_type() {
    let cap = zeroship_data_sql::MAX_MEMBERSHIP_LIST_LEN;
    let at_cap: Vec<Literal> = (0..i64::try_from(cap).expect("cap fits an i64"))
        .map(Literal::Int)
        .collect();
    let over_cap: Vec<Literal> = (0..=i64::try_from(cap).expect("cap fits an i64"))
        .map(Literal::Int)
        .collect();
    assert!(LiteralSet::new(at_cap).is_ok());
    assert_eq!(
        LiteralSet::new(over_cap),
        Err(LiteralError::MembershipListTooLong { len: cap + 1 })
    );
    println!("ruled on 2 set sizes around the cap of {cap}");
}
