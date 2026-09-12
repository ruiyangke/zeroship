//! A plan carrying a parameter renders a placeholder, never the value.
//!
//! This is the property that makes the prepared-statement cache reachable at
//! all, and it is also the security property: a value that never enters the
//! statement cannot be parsed as SQL, whatever it contains.
//!
//! The arm to be careful about is the second one. Asserting that the SQL
//! *contains* `$1` proves a placeholder was emitted; it does not prove the
//! value was not ALSO emitted. So each vector below is checked for the absence
//! of its own text, with vectors chosen so that absence is meaningful - a value
//! that looks like SQL, a value that looks like a quote, a value that looks
//! like a comment.

use zeroship_data_orm::sql::render::postgres;
use zeroship_data_orm::sql::{
    ArithmeticOp, Assignment, BindBudget, ColumnAssignment, ColumnValue, CompareOp, Delete,
    EscapeChar, Ident, IdentRole, Insert, Literal, MembershipOp, Operand, PatternOp, Predicate,
    ProjectedField, Projection, Returning, RowLimit, RowOffset, Select, TextPattern, Update,
    WriteValue,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn users() -> Ident {
    Ident::parse_as("users", IdentRole::Collection).expect("collection")
}

fn read_with(filter: Predicate) -> zeroship_data_orm::sql::RenderedSql {
    let projection = Projection::rows(vec![
        ProjectedField::column(column("name")).expect("projectable"),
    ])
    .expect("row projection");
    let plan = Select::builder(
        Ident::parse_as("users", IdentRole::Collection).expect("collection"),
        projection,
    )
    .filter(filter)
    .limit(RowLimit::new(10).expect("limit"))
    .offset(RowOffset::new(20).expect("offset"))
    .build()
    .expect("valid plan");
    postgres::render_select(&plan).expect("renders")
}

/// Hostile-looking values must reach the parameter list and nothing else.
#[test]
fn a_value_reaches_the_parameter_list_and_never_the_statement() {
    let vectors = [
        "'; DROP TABLE users; --",
        "\" OR \"1\"=\"1",
        "/* comment */",
        "Robert'); DROP TABLE students;--",
        "%",
        "abc",
    ];
    let mut ruled_on = 0_usize;
    for vector in vectors {
        let rendered = read_with(Predicate::compare(
            Operand::column(column("name")),
            CompareOp::Eq,
            Operand::Lit(Literal::text(vector).expect("text")),
        ));
        assert!(
            !rendered.sql().contains(vector),
            "the value {vector:?} appeared in the statement: {}",
            rendered.sql()
        );
        assert!(
            rendered.sql().contains(r#""name" = $1"#),
            "expected a placeholder, got: {}",
            rendered.sql()
        );
        assert_eq!(
            rendered.params().first(),
            Some(&Literal::Text(vector.to_string())),
            "the value did not reach the parameter list"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, vectors.len());
    assert!(ruled_on >= 5, "ruled on {ruled_on} values");
    println!("ruled on {ruled_on} hostile values");
}

/// SC-3's third named invariant that the shapes cannot enforce: a plan's
/// parameter count must equal the placeholders its lowering emits. Counted from
/// the SQL rather than from the renderer's bookkeeping, which is the side that
/// would be wrong if the bookkeeping were.
#[test]
fn every_placeholder_has_exactly_one_parameter() {
    let plans = [
        Predicate::always(),
        Predicate::compare(
            Operand::column(column("age")),
            CompareOp::Gte,
            Operand::Lit(Literal::Int(18)),
        ),
        Predicate::membership(
            Operand::column(column("status")),
            MembershipOp::In,
            vec![
                Some(Literal::Int(1)),
                Some(Literal::Int(2)),
                Some(Literal::Int(3)),
            ],
        )
        .expect("membership"),
        Predicate::membership(
            Operand::column(column("status")),
            MembershipOp::NotIn,
            vec![Some(Literal::Int(1)), None],
        )
        .expect("membership"),
        Predicate::Pattern {
            lhs: Operand::column(column("name")),
            op: PatternOp::Like,
            pattern: TextPattern::new("a%").expect("pattern"),
            escape: Some(EscapeChar::new('\\').expect("escape")),
        },
        Predicate::range(
            Operand::column(column("age")),
            Operand::Lit(Literal::Int(18)),
            Operand::Lit(Literal::Int(65)),
            zeroship_data_orm::sql::RangeBounds::InclusiveBoth,
        ),
        Predicate::is_null(Operand::column(column("deleted_at"))),
    ];
    let mut ruled_on = 0_usize;
    let mut with_params = 0_usize;
    for filter in plans {
        let rendered = read_with(filter);
        assert_eq!(
            rendered.placeholder_count(),
            rendered.params().len(),
            "placeholder/parameter mismatch in: {}",
            rendered.sql()
        );
        // Limit and offset are always bound, so no plan has zero parameters.
        assert!(rendered.params().len() >= 2);
        if rendered.params().len() > 2 {
            with_params += 1;
        }
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 7);
    assert!(
        with_params >= 5,
        "only {with_params} of {ruled_on} plans bound a filter value, so the arm is \
         mostly ruling on LIMIT/OFFSET"
    );
    println!("ruled on {ruled_on} plans, {with_params} of them carrying filter values");
}

/// Pagination values are parameters too. This is not tidiness: binding them is
/// what lets one prepared statement serve every page instead of one per offset.
#[test]
fn limit_and_offset_are_bound_not_interpolated() {
    let rendered = read_with(Predicate::always());
    assert!(
        rendered.sql().ends_with("LIMIT $1 OFFSET $2"),
        "unexpected pagination lowering: {}",
        rendered.sql()
    );
    assert!(
        !rendered.sql().contains("LIMIT 10"),
        "the limit was interpolated: {}",
        rendered.sql()
    );
    assert_eq!(
        rendered.params(),
        &[Literal::Int(10), Literal::Int(20)],
        "pagination values did not reach the parameter list"
    );
    println!("ruled on 1 paginated plan");
}

/// A BINARY value is a typed variant, not a tagged string.
///
/// A typed renderer must preserve bytes in its parameter list, with neither
/// a value prefix nor a decoding wrapper in SQL.
#[test]
fn a_binary_value_is_a_typed_parameter_with_no_wrapper_and_no_sentinel() {
    let payload = vec![
        0x00_u8, 0xff, 0x10, b'_', b'_', b'z', b's', b'b', b'i', b'n',
    ];
    let rendered = read_with(Predicate::compare(
        Operand::column(column("blob")),
        CompareOp::Eq,
        Operand::Lit(Literal::Bytes(payload.clone())),
    ));

    assert!(
        rendered.sql().contains(r#""blob" = $1"#),
        "a binary parameter must render as a bare placeholder: {}",
        rendered.sql()
    );
    for smuggling in ["decode(", "base64", "bytea", "FROM_BASE64", "__zsbin"] {
        assert!(
            !rendered.sql().contains(smuggling),
            "the statement still carries the {smuggling:?} smuggling mechanism: {}",
            rendered.sql()
        );
    }
    assert_eq!(
        rendered.params().first(),
        Some(&Literal::Bytes(payload)),
        "the bytes must arrive as bytes, not as a tagged string"
    );
    println!("ruled on 1 binary parameter and 5 absent mechanisms");
}

/// The same property on the WRITE side, where the value is not a filter operand
/// but the thing being stored - the position a builder is most tempted to
/// interpolate, because it looks like data going in rather than a predicate.
#[test]
fn a_written_value_reaches_the_parameter_list_and_never_the_statement() {
    let vectors = [
        "'; DROP TABLE users; --",
        "\" OR \"1\"=\"1",
        "/* comment */",
        "NULL",
        "%",
        "abc",
    ];
    let mut ruled_on = 0_usize;
    for vector in vectors {
        let value = Literal::text(vector).expect("text");

        let insert = Insert::builder(users(), Returning::nothing(), BindBudget::POSTGRES)
            .row(vec![ColumnValue::new(
                column("name"),
                WriteValue::Bind(value.clone()),
            )])
            .build()
            .expect("valid insert");
        let update = Update::builder(
            users(),
            zeroship_data_orm::sql::Ident::parse_as("id", zeroship_data_orm::sql::IdentRole::Column).unwrap(),
            RowLimit::default(),
            Returning::nothing(),
        )
        .set(ColumnAssignment::new(
            column("name"),
            Assignment::bind(value.clone()),
        ))
        .build()
        .expect("valid update");

        for rendered in [
            postgres::render_insert(&insert).expect("renders"),
            postgres::render_update(&update).expect("renders"),
        ] {
            assert!(
                !rendered.sql().contains(vector),
                "the value {vector:?} appeared in the statement: {}",
                rendered.sql()
            );
            assert_eq!(
                rendered.params().first(),
                Some(&value),
                "the value did not reach the parameter list"
            );
            ruled_on += 1;
        }
    }
    assert_eq!(ruled_on, vectors.len() * 2);
    assert!(ruled_on >= 10, "ruled on {ruled_on} written values");
    println!("ruled on {ruled_on} hostile written values");
}

/// A written `"NULL"` STRING must reach the parameter list, while a
/// [`WriteValue::Null`] node reaches the statement as a keyword. The two are
/// distinguishable in the rendered form, which is the property that makes
/// nullness-as-a-node worth having: with a `Literal::Null` variant, the string
/// and the node would be one type and the renderer would have to tell them
/// apart by value.
#[test]
fn a_null_shaped_string_is_a_parameter_and_a_null_node_is_a_keyword() {
    let build = |value: WriteValue| {
        let insert = Insert::builder(users(), Returning::nothing(), BindBudget::POSTGRES)
            .row(vec![ColumnValue::new(column("name"), value)])
            .build()
            .expect("valid insert");
        postgres::render_insert(&insert).expect("renders")
    };

    let as_string = build(WriteValue::Bind(Literal::text("NULL").expect("text")));
    assert!(
        as_string.sql().ends_with("VALUES ($1)"),
        "{}",
        as_string.sql()
    );
    assert_eq!(
        as_string.params(),
        &[Literal::Text("NULL".to_string())],
        "a string that spells NULL is still a value"
    );

    let as_node = build(WriteValue::Null);
    assert!(
        as_node.sql().ends_with("VALUES (NULL)"),
        "{}",
        as_node.sql()
    );
    assert!(
        as_node.params().is_empty(),
        "the NULL keyword must bind nothing"
    );

    assert_ne!(as_string.sql(), as_node.sql());
    println!("ruled on 1 null string and 1 null node");
}

/// SC-3's third named invariant, applied to every write shape: a plan's
/// parameter count equals the placeholders its lowering emits. Counted from the
/// SQL, not from the renderer's own bookkeeping.
#[test]
fn every_placeholder_has_exactly_one_parameter_on_a_write() {
    let filter = Predicate::compare(
        Operand::column(column("status")),
        CompareOp::Eq,
        Operand::Lit(Literal::Int(1)),
    );
    let returning = Returning::rows(
        Projection::rows(vec![
            ProjectedField::column(column("name")).expect("projectable"),
        ])
        .expect("row projection"),
    )
    .expect("returning");

    let insert = Insert::builder(users(), returning.clone(), BindBudget::POSTGRES)
        .row(vec![
            ColumnValue::new(column("a"), WriteValue::Bind(Literal::Int(1))),
            ColumnValue::new(column("b"), WriteValue::Null),
        ])
        .row(vec![
            ColumnValue::new(column("a"), WriteValue::Null),
            ColumnValue::new(column("b"), WriteValue::Bind(Literal::Int(2))),
        ])
        .build()
        .expect("valid insert");
    let update = Update::builder(
        users(),
        zeroship_data_orm::sql::Ident::parse_as("id", zeroship_data_orm::sql::IdentRole::Column).unwrap(),
        RowLimit::default(),
        returning.clone(),
    )
    .set(ColumnAssignment::new(
        column("a"),
        Assignment::bind(Literal::Int(1)),
    ))
    .set(ColumnAssignment::new(column("b"), Assignment::null()))
    .set(ColumnAssignment::new(
        column("c"),
        Assignment::arithmetic(ArithmeticOp::Add, Literal::Int(1)).expect("numeric"),
    ))
    .set(ColumnAssignment::new(
        column("d"),
        Assignment::CurrentTimestamp,
    ))
    .filter(filter.clone())
    .build()
    .expect("valid update");
    let delete = Delete::builder(
        users(),
        zeroship_data_orm::sql::Ident::parse_as("id", zeroship_data_orm::sql::IdentRole::Column).unwrap(),
        RowLimit::default(),
        returning,
    )
    .filter(filter)
    .build()
    .expect("valid delete");

    let rendered = [
        postgres::render_insert(&insert).expect("renders"),
        postgres::render_update(&update).expect("renders"),
        postgres::render_delete(&delete).expect("renders"),
    ];
    let mut ruled_on = 0_usize;
    for one in &rendered {
        assert_eq!(
            one.placeholder_count(),
            one.params().len(),
            "placeholder/parameter mismatch in: {}",
            one.sql()
        );
        ruled_on += 1;
    }
    // The counts are asserted individually too, because "they agree" would also
    // hold if both were zero.
    assert_eq!(rendered[0].params().len(), 2, "two of four cells are null");
    assert_eq!(
        rendered[1].params().len(),
        4,
        "one value, one arithmetic operand, one filter value, one bound"
    );
    assert_eq!(rendered[2].params().len(), 2, "one filter value, one bound");
    assert_eq!(ruled_on, 3);
    println!("ruled on {ruled_on} write statements");
}

/// A `LIKE` escape character is a value, and it is the one place in a pattern
/// lowering where caller-chosen text would otherwise be interpolated into the
/// statement as a quoted literal.
#[test]
fn a_like_escape_character_is_bound() {
    let rendered = read_with(Predicate::Pattern {
        lhs: Operand::column(column("name")),
        op: PatternOp::Like,
        pattern: TextPattern::new("100!%").expect("pattern"),
        escape: Some(EscapeChar::new('!').expect("escape")),
    });
    assert!(
        rendered.sql().contains(r#""name" LIKE $1 ESCAPE $2"#),
        "unexpected pattern lowering: {}",
        rendered.sql()
    );
    assert!(
        !rendered.sql().contains('!'),
        "the escape character was interpolated: {}",
        rendered.sql()
    );
    assert_eq!(rendered.params()[0], Literal::Text("100!%".to_string()));
    assert_eq!(rendered.params()[1], Literal::Text("!".to_string()));
    println!("ruled on 1 escaped pattern");
}
