//! The write family's own invariants: what a write returns, what bounds it, and
//! what it refuses.
//!
//! Every arm declares the number of items IT RULED ON and a floor that number
//! must clear, following the repository's gate discipline: a check that examines
//! nothing and a clean tree print the same thing.

use zeroship_data_query_builder::render::postgres::{self, RenderError};
use zeroship_data_query_builder::{
    AggregateRef, Arithmetic, ArithmeticOp, Assignment, BindBudget, ColumnAssignment, ColumnValue,
    CompareOp, Delete, FieldPath, Ident, IdentRole, Insert, JsonKey, Literal, Operand, PlanError,
    Predicate, ProjectedField, Projection, Returning, RowLimit, Update, WriteError, WriteValue,
    MAX_INSERT_ROWS, MAX_ROW_LIMIT, PLATFORM_FIELD_NAMES,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn alias(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Alias).expect("valid alias")
}

fn collection() -> Ident {
    Ident::parse_as("users", IdentRole::Collection).expect("valid collection")
}

fn field(name: &str) -> ProjectedField {
    ProjectedField::column(column(name)).expect("projectable")
}

/// A `RETURNING` list of one declared column plus the platform union.
fn returning_name() -> Returning {
    Returning::rows(Projection::rows(vec![field("name")]).expect("row projection"))
        .expect("returning")
}

fn cell(name: &str, value: i64) -> ColumnValue {
    ColumnValue::new(column(name), WriteValue::Bind(Literal::Int(value)))
}

fn insert_one(returning: Returning) -> Insert {
    Insert::builder(collection(), returning, BindBudget::POSTGRES)
        .row(vec![cell("name", 1), cell("email", 2)])
        .build()
        .expect("valid insert")
}

fn update_one(returning: Returning) -> Update {
    Update::builder(collection(), RowLimit::default(), returning)
        .set(ColumnAssignment::new(
            column("name"),
            Assignment::bind(Literal::Int(1)),
        ))
        .build()
        .expect("valid update")
}

fn delete_one(returning: Returning) -> Delete {
    Delete::builder(collection(), RowLimit::default(), returning)
        .build()
        .expect("valid delete")
}

fn sql_of(plan: &zeroship_data_query_builder::DbPlan) -> String {
    postgres::render(plan).expect("renders").sql().to_string()
}

fn all_three_writes(returning: &Returning) -> Vec<zeroship_data_query_builder::DbPlan> {
    vec![
        zeroship_data_query_builder::DbPlan::Insert(insert_one(returning.clone())),
        zeroship_data_query_builder::DbPlan::Update(update_one(returning.clone())),
        zeroship_data_query_builder::DbPlan::Delete(delete_one(returning.clone())),
    ]
}

// ---------------------------------------------------------------------------
// RETURNING
// ---------------------------------------------------------------------------

/// THE CRUX. `RETURNING *` returns every PHYSICAL column, which is not the set
/// the creator may see, and it is what all twelve production write sites emit
/// (`query.rs:3584`, `:4005`, `:4157`, `:4228`, `:4254`, `:4290`, `:4445`,
/// `:4483`, `:4525`, `:4557`, `:5937`, `:6020`).
///
/// The absence here is structural rather than remembered: `Returning` wraps a
/// `Projection`, `ProjectionSource` has no wildcard variant, and there is no
/// "return everything" constructor. This arm rules on the rendered statement,
/// which is the only place a star could appear.
#[test]
fn no_write_renders_a_returning_star() {
    let returning = returning_name();
    let mut ruled_on = 0_usize;
    for plan in all_three_writes(&returning) {
        let sql = sql_of(&plan);
        assert!(
            !sql.contains("RETURNING *"),
            "a star reached a RETURNING list: {sql}"
        );
        // The same two forms the read family's `no_projection_renders_a_wildcard`
        // rules out. A bare `contains('*')` would be WRONG here rather than
        // merely stricter: `Assignment::Arithmetic` with `ArithmeticOp::Multiply`
        // legitimately renders `"score" = "score" * $1`, so that arm would fail
        // on a correct statement the moment a fixture used it.
        assert!(
            !sql.contains(".*"),
            "a qualified wildcard reached the statement: {sql}"
        );
        // Not merely "no star": the explicit list must actually be there, or a
        // renderer that dropped the clause entirely would pass the arm above.
        assert!(
            sql.contains(r#"RETURNING "created_at" AS "created_at""#),
            "the explicit RETURNING list is missing: {sql}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 3, "insert, update and delete must all be ruled on");
    println!("ruled on {ruled_on} write statements");
}

/// THE ARM THAT MATTERS, asserted on rendered SQL.
///
/// On the READ path the SQL substitutes the sibling and the plaintext never
/// leaves the database. On the WRITE path today it does leave: `RETURNING *`
/// hands back the parent and the sibling both, and a Rust pass in the worker
/// replaces one with the other afterwards.
/// `crud/mask_pass.rs:913-937` is a test whose fixture states that shape - a row
/// carrying `"ssn": "BASE64CIPHERTEXT"` alongside `"ssn_masked": "***-**-6789"`.
///
/// With `Returning` over a `Projection`, the substitution is the same node the
/// read family uses, so the plaintext column is never named.
#[test]
fn a_returning_list_over_a_stored_column_renders_the_physical_name() {
    let returning = Returning::rows(
        Projection::rows(vec![ProjectedField::stored(
            Ident::parse_as("__zs_raw__ssn", IdentRole::StoredColumn).expect("stored column"),
            column("ssn"),
        )
        .expect("stored field")])
        .expect("row projection"),
    )
    .expect("returning");

    let mut ruled_on = 0_usize;
    for plan in all_three_writes(&returning) {
        let sql = sql_of(&plan);
        assert!(
            sql.contains(r#""__zs_raw__ssn" AS "ssn""#),
            "the storage substitution did not reach the RETURNING list: {sql}"
        );
        // The logical name is NOT returned. Checking the exact projected form
        // is what distinguishes "the physical column was returned" from "both".
        assert!(
            !sql.contains(r#""ssn" AS "ssn""#),
            "the plaintext column was returned alongside the stored one: {sql}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 3);
    println!("ruled on {ruled_on} stored write statements");
}

/// The platform-field union reaches a write's returned row too, which is what
/// keeps `id` present - and `id` is what the change-event publication
/// correlates on, what the unmask handle plucks `row_pk` from, and what an
/// encrypted column binds into its AEAD tag.
#[test]
fn a_returning_list_keeps_every_platform_field() {
    let sql = sql_of(&zeroship_data_query_builder::DbPlan::Insert(insert_one(
        returning_name(),
    )));
    let mut ruled_on = 0_usize;
    for required in PLATFORM_FIELD_NAMES {
        assert!(
            sql.contains(&format!(r#""{required}" AS "{required}""#)),
            "the RETURNING list dropped the platform field {required}: {sql}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 7);
    println!("ruled on {ruled_on} platform fields in a RETURNING list");
}

/// A write returns the rows it touched, so a `RETURNING` list cannot aggregate.
/// The refusal is at the type's constructor, and the field is private, so a
/// caller cannot build the variant around it.
#[test]
fn an_aggregate_returning_is_refused() {
    let aggregate = Projection::aggregate(vec![ProjectedField::aggregate(
        AggregateRef::count_rows(),
        alias("total"),
    )])
    .expect("aggregate projection");
    assert_eq!(
        Returning::rows(aggregate).expect_err("must refuse"),
        WriteError::AggregateReturning
    );
    // The control: the row projection the same call site would otherwise build
    // must be accepted, or the refusal is a blanket ban.
    assert!(Returning::rows(Projection::rows(vec![field("name")]).expect("rows")).is_ok());
    println!("ruled on 2 projections");
}

/// `Returning::nothing()` is the NARROW end of the type, and it must really
/// emit no clause - a write that returns nothing is the right shape for a bulk
/// purge, which today counts `RETURNING *` rows to obtain a number the driver
/// already has.
#[test]
fn a_write_that_returns_nothing_emits_no_returning_clause() {
    let nothing = Returning::nothing();
    assert!(nothing.projection().is_none());
    let mut ruled_on = 0_usize;
    for plan in all_three_writes(&nothing) {
        let sql = sql_of(&plan);
        assert!(
            !sql.contains("RETURNING"),
            "a RETURNING clause was emitted for a write that returns nothing: {sql}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 3);
    println!("ruled on {ruled_on} count-only writes");
}

// ---------------------------------------------------------------------------
// The bind budget
// ---------------------------------------------------------------------------

/// A batch is bounded by BINDS, not by document count - the bug that already
/// shipped once. This arm builds a batch that is comfortably inside
/// `MAX_INSERT_ROWS` and outside the bind budget, so the document cap cannot be
/// what refuses it.
///
/// The control is the same shape one column narrower, which must be accepted:
/// without it the arm would also pass if the builder refused everything.
#[test]
fn an_insert_is_bounded_by_binds_rather_than_by_documents() {
    let budget = BindBudget::SQLITE;
    let rows = 100_usize;
    // 100 rows is a tenth of the document cap; the bind count is what decides.
    let over_width = budget.max() / rows + 1;
    let at_width = budget.max() / rows;

    let build = |width: usize| {
        let row: Vec<ColumnValue> = (0..width)
            .map(|i| ColumnValue::new(column(&format!("c{i}")), WriteValue::Bind(Literal::Int(1))))
            .collect();
        let mut builder = Insert::builder(collection(), Returning::nothing(), budget);
        for _ in 0..rows {
            builder = builder.row(row.clone());
        }
        builder.build()
    };

    let refused = build(over_width).expect_err("must refuse");
    assert_eq!(
        refused,
        WriteError::BindBudgetExceeded {
            binds: rows * over_width,
            budget,
        },
        "the refusal must name the bind count, not the document count"
    );
    assert!(
        rows < MAX_INSERT_ROWS,
        "the batch must be inside the document cap or this arm is measuring the wrong bound"
    );

    let accepted = build(at_width).expect("at the budget");
    assert_eq!(accepted.bind_count(), rows * at_width);
    assert!(accepted.bind_count() <= budget.max());
    println!(
        "ruled on 2 batch widths of {rows} rows around the {} budget of {}",
        budget.dialect_name(),
        budget.max()
    );
}

/// The bound is PER DIALECT, and the two dialects must actually disagree about
/// the same batch - otherwise "the budget is per dialect" is a claim no test
/// distinguishes from a single constant.
#[test]
fn the_same_batch_is_accepted_by_one_dialect_and_refused_by_the_other() {
    let rows = 100_usize;
    // Between the two ceilings: over SQLite's 32,766, under Postgres' 65,535.
    let width = 400_usize;
    assert!(rows * width > BindBudget::SQLITE.max());
    assert!(rows * width <= BindBudget::POSTGRES.max());

    let build = |budget: BindBudget| {
        let row: Vec<ColumnValue> = (0..width)
            .map(|i| ColumnValue::new(column(&format!("c{i}")), WriteValue::Bind(Literal::Int(1))))
            .collect();
        let mut builder = Insert::builder(collection(), Returning::nothing(), budget);
        for _ in 0..rows {
            builder = builder.row(row.clone());
        }
        builder.build()
    };

    assert!(build(BindBudget::POSTGRES).is_ok());
    assert!(matches!(
        build(BindBudget::SQLITE),
        Err(WriteError::BindBudgetExceeded { .. })
    ));
    println!("ruled on 1 batch against 2 dialect budgets");
}

/// The document cap and the bind budget are SEPARATE bounds, which is what
/// `query.rs:598-600` says in its own comment and what a single cap cannot
/// express. This batch is one row over the document cap and nowhere near either
/// bind budget.
#[test]
fn the_document_cap_is_a_separate_bound_from_the_bind_budget() {
    let mut builder = Insert::builder(collection(), Returning::nothing(), BindBudget::SQLITE);
    for _ in 0..=MAX_INSERT_ROWS {
        builder = builder.row(vec![cell("n", 1)]);
    }
    let refused = builder.build().expect_err("must refuse");
    assert_eq!(
        refused,
        WriteError::BatchTooLarge {
            rows: MAX_INSERT_ROWS + 1
        }
    );
    // One bind per row, so the bind budget is not what refused it.
    assert!(MAX_INSERT_ROWS + 1 < BindBudget::SQLITE.max());

    // The control at the cap.
    let mut builder = Insert::builder(collection(), Returning::nothing(), BindBudget::SQLITE);
    for _ in 0..MAX_INSERT_ROWS {
        builder = builder.row(vec![cell("n", 1)]);
    }
    assert!(builder.build().is_ok());
    println!("ruled on 2 batch sizes around the document cap of {MAX_INSERT_ROWS}");
}

/// The budget counts PARAMETERS, not cells: a null binds nothing. A batch whose
/// cell count is over the budget and whose bind count is under it must be
/// accepted, or the bound is measuring the wrong resource.
#[test]
fn the_budget_counts_parameters_and_a_null_is_not_one() {
    let budget = BindBudget::SQLITE;
    let rows = 100_usize;
    let width = 400_usize;
    assert!(rows * width > budget.max(), "the CELL count must be over");

    // Every other column is a null, halving the bind count.
    let row: Vec<ColumnValue> = (0..width)
        .map(|i| {
            let value = if i % 2 == 0 {
                WriteValue::Bind(Literal::Int(1))
            } else {
                WriteValue::Null
            };
            ColumnValue::new(column(&format!("c{i}")), value)
        })
        .collect();
    let mut builder = Insert::builder(collection(), Returning::nothing(), budget);
    for _ in 0..rows {
        builder = builder.row(row.clone());
    }
    let insert = builder.build().expect("binds are under the budget");
    assert_eq!(insert.bind_count(), rows * width / 2);
    assert!(insert.bind_count() <= budget.max());

    let rendered = postgres::render_insert(&insert).expect("renders");
    assert_eq!(
        rendered.placeholder_count(),
        rendered.params().len(),
        "the statement's placeholders and the parameter list disagree"
    );
    assert_eq!(rendered.params().len(), insert.bind_count());
    println!(
        "ruled on 1 batch of {} cells carrying {} parameters",
        rows * width,
        insert.bind_count()
    );
}

// ---------------------------------------------------------------------------
// Bounds on UPDATE and DELETE
// ---------------------------------------------------------------------------

/// An unbounded update has no representation.
///
/// The two shapes that are unbounded today are both covered: an EMPTY filter,
/// which `query.rs:4223-4228` renders with no `WHERE` clause at all, and any
/// filter at all, which it renders with no bound.
#[test]
fn no_update_or_delete_is_unbounded() {
    let filters = [
        Predicate::always(),
        Predicate::compare(
            Operand::column(column("status")),
            CompareOp::Eq,
            Operand::Lit(Literal::Int(1)),
        ),
    ];
    let mut ruled_on = 0_usize;
    for filter in filters {
        let update = Update::builder(collection(), RowLimit::default(), Returning::nothing())
            .set(ColumnAssignment::new(
                column("name"),
                Assignment::bind(Literal::Int(1)),
            ))
            .filter(filter.clone())
            .build()
            .expect("valid update");
        let delete = Delete::builder(collection(), RowLimit::default(), Returning::nothing())
            .filter(filter)
            .build()
            .expect("valid delete");

        for sql in [
            postgres::render_update(&update).expect("renders").sql().to_string(),
            postgres::render_delete(&delete).expect("renders").sql().to_string(),
        ] {
            assert!(
                sql.contains(r#"WHERE "id" IN (SELECT "id" FROM"#),
                "the bounded-target subquery is missing: {sql}"
            );
            assert!(
                sql.contains("LIMIT $"),
                "a write rendered without a bound: {sql}"
            );
            assert!(
                sql.contains("FOR UPDATE)"),
                "the bounded target was chosen without locking the rows it chose: {sql}"
            );
            ruled_on += 1;
        }
    }
    assert_eq!(ruled_on, 4);
    println!("ruled on {ruled_on} bounded writes, 2 of them with an empty filter");
}

/// The bound is the same [`RowLimit`] the read family uses, so it carries the
/// same cap and the same refusal - and it defaults to the cap rather than to
/// absence.
#[test]
fn the_write_bound_is_capped_and_defaults_to_the_cap() {
    assert_eq!(RowLimit::default().get(), MAX_ROW_LIMIT);
    let update = Update::builder(collection(), RowLimit::default(), Returning::nothing())
        .set(ColumnAssignment::new(
            column("name"),
            Assignment::bind(Literal::Int(1)),
        ))
        .build()
        .expect("valid update");
    assert_eq!(update.limit().get(), MAX_ROW_LIMIT);
    let rendered = postgres::render_update(&update).expect("renders");
    assert_eq!(
        rendered.params().last(),
        Some(&Literal::Int(MAX_ROW_LIMIT)),
        "the bound must be bound as a parameter, not interpolated"
    );

    let mut ruled_on = 0_usize;
    for bad in [0_i64, -1, MAX_ROW_LIMIT + 1] {
        assert_eq!(
            RowLimit::new(bad).expect_err("must refuse"),
            PlanError::LimitOutOfRange { requested: bad }
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 3);
    println!("ruled on 1 default and {ruled_on} refused bounds");
}

/// A single-row update is the same node with a bound of one, so `updateOne` and
/// `updateMany` do not need two plans - and the `LIMIT 1` narrowing that
/// `query.rs:4004-4007` hand-writes falls out of the general shape.
#[test]
fn a_single_row_update_is_the_same_node_bounded_to_one() {
    let update = Update::builder(
        collection(),
        RowLimit::new(1).expect("limit"),
        Returning::nothing(),
    )
    .set(ColumnAssignment::new(
        column("name"),
        Assignment::bind(Literal::Int(1)),
    ))
    .build()
    .expect("valid update");
    let rendered = postgres::render_update(&update).expect("renders");
    assert!(rendered.sql().contains("LIMIT $2 FOR UPDATE)"));
    assert_eq!(rendered.params()[1], Literal::Int(1));
    println!("ruled on 1 single-row update");
}

// ---------------------------------------------------------------------------
// NULL, arithmetic and the server clock
// ---------------------------------------------------------------------------

/// A written NULL is a KEYWORD, not a parameter: it consumes no placeholder and
/// no slot in the parameter list. That is the whole reason `Literal` needs no
/// null variant.
#[test]
fn a_written_null_renders_the_keyword_and_binds_nothing() {
    let insert = Insert::builder(collection(), Returning::nothing(), BindBudget::POSTGRES)
        .row(vec![
            ColumnValue::new(column("name"), WriteValue::Bind(Literal::Int(7))),
            ColumnValue::new(column("note"), WriteValue::Null),
        ])
        .build()
        .expect("valid insert");
    let rendered = postgres::render_insert(&insert).expect("renders");
    assert_eq!(
        rendered.sql(),
        r#"INSERT INTO "users" ("name", "note") VALUES ($1, NULL)"#
    );
    assert_eq!(rendered.params(), &[Literal::Int(7)]);
    assert_eq!(rendered.placeholder_count(), 1);

    // And on the UPDATE side.
    let update = Update::builder(collection(), RowLimit::default(), Returning::nothing())
        .set(ColumnAssignment::new(column("note"), Assignment::null()))
        .build()
        .expect("valid update");
    let rendered = postgres::render_update(&update).expect("renders");
    assert!(rendered.sql().contains(r#"SET "note" = NULL WHERE"#));
    // Only the bound remains as a parameter.
    assert_eq!(rendered.params(), &[Literal::Int(MAX_ROW_LIMIT)]);
    println!("ruled on 2 written nulls");
}

/// The conversion boundary. A caller holding a nullable value cannot reach a
/// `Bind` without handling the `None` arm, and the `None` arm is a node.
#[test]
fn the_nullable_conversion_boundary_produces_a_node() {
    assert_eq!(WriteValue::from_optional(None), WriteValue::Null);
    assert_eq!(
        WriteValue::from_optional(Some(Literal::Int(1))),
        WriteValue::Bind(Literal::Int(1))
    );
    assert!(WriteValue::from_optional(None).is_null());
    println!("ruled on 2 conversions");
}

/// `"version" = "version" + 1` is what `query.rs:3907` emits on every dispatched
/// update, and `"updated_at" = NOW()` is what `query.rs:3910-3917` emits. A
/// write family that cannot render both cannot replace the builder, so both
/// shapes are asserted on the statement.
///
/// The `::numeric` cast the shipped lowering carries (`query.rs:3816`) must be
/// ABSENT: it exists because that parameter is a `String`, and a typed integer
/// gives it nothing to repair.
#[test]
fn the_platform_system_field_bumps_both_render() {
    let update = Update::builder(collection(), RowLimit::default(), Returning::nothing())
        .set(ColumnAssignment::new(
            column("version"),
            Assignment::arithmetic(ArithmeticOp::Add, Literal::Int(1)).expect("numeric"),
        ))
        .set(ColumnAssignment::new(
            column("updated_at"),
            Assignment::CurrentTimestamp,
        ))
        .build()
        .expect("valid update");
    let rendered = postgres::render_update(&update).expect("renders");
    let sql = rendered.sql();
    assert!(
        sql.contains(r#"SET "updated_at" = NOW(), "version" = "version" + $1"#),
        "unexpected system-field lowering: {sql}"
    );
    assert!(
        !sql.contains("::numeric"),
        "the cast survived a typed parameter: {sql}"
    );
    assert_eq!(rendered.params()[0], Literal::Int(1));
    // The clock is the SERVER's: it is an expression, not a bound value.
    assert_eq!(rendered.params().len(), 2, "NOW() must bind nothing");
    println!("ruled on 1 version bump and 1 server-clock stamp");
}

/// `col = col + $1` needs a numeric operand. The shipped lowering pushes
/// whatever `value_to_param` returns (`query.rs:3814-3825`) and finds out at
/// execution, against a statement the creator never wrote.
#[test]
fn arithmetic_refuses_a_non_numeric_operand() {
    let mut ruled_on = 0_usize;
    for (value, expected) in [
        (Literal::text("x").expect("text"), "text"),
        (Literal::Bool(true), "bool"),
        (Literal::Bytes(vec![1]), "bytes"),
    ] {
        assert_eq!(
            Arithmetic::new(ArithmeticOp::Add, value).expect_err("must refuse"),
            WriteError::ArithmeticOperandNotNumeric { found: expected }
        );
        ruled_on += 1;
    }
    // The controls: both numeric literals are accepted, or the refusal is a
    // blanket ban rather than a type check.
    for value in [Literal::Int(1), Literal::float(1.5).expect("finite")] {
        assert!(Arithmetic::new(ArithmeticOp::Multiply, value).is_ok());
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 5);
    println!("ruled on {ruled_on} arithmetic operands");
}

/// The arithmetic node's left operand is the column being assigned and cannot
/// be anything else, which is what keeps a general expression grammar out of
/// the write family. This arm rules on the rendered form, where a second column
/// would have to appear.
#[test]
fn arithmetic_can_only_reference_the_column_it_assigns() {
    let mut ruled_on = 0_usize;
    for (op, symbol) in [
        (ArithmeticOp::Add, "+"),
        (ArithmeticOp::Subtract, "-"),
        (ArithmeticOp::Multiply, "*"),
    ] {
        let update = Update::builder(collection(), RowLimit::default(), Returning::nothing())
            .set(ColumnAssignment::new(
                column("score"),
                Assignment::arithmetic(op, Literal::Int(3)).expect("numeric"),
            ))
            .build()
            .expect("valid update");
        let sql = postgres::render_update(&update)
            .expect("renders")
            .sql()
            .to_string();
        assert!(
            sql.contains(&format!(r#"SET "score" = "score" {symbol} $1"#)),
            "unexpected arithmetic lowering for {op:?}: {sql}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 3);
    println!("ruled on {ruled_on} arithmetic operators");
}

// ---------------------------------------------------------------------------
// The schema-free boundary
// ---------------------------------------------------------------------------

/// A gap in a batch is REFUSED rather than filled with a SQL `NULL`.
///
/// The shipped builder unions the columns across documents and fills every gap
/// with `NULL` (`query.rs:4119-4135`), which silently overrides the column's DDL
/// default - and the platform declares one for `created_at` and `updated_at`
/// itself (`query.rs:212`). Whether a gap means NULL or "use the default"
/// depends on the declared schema, which this crate does not have.
#[test]
fn a_column_set_mismatch_is_refused_rather_than_filled_with_null() {
    let refused = Insert::builder(collection(), Returning::nothing(), BindBudget::POSTGRES)
        .row(vec![cell("a", 1), cell("b", 2)])
        .row(vec![cell("a", 3)])
        .build()
        .expect_err("must refuse");
    assert_eq!(refused, WriteError::ColumnSetMismatch { row: 1 });

    // A same-width row that names a DIFFERENT column is also a mismatch: the
    // widths matching is not the property.
    let refused = Insert::builder(collection(), Returning::nothing(), BindBudget::POSTGRES)
        .row(vec![cell("a", 1), cell("b", 2)])
        .row(vec![cell("a", 3), cell("c", 4)])
        .build()
        .expect_err("must refuse");
    assert_eq!(refused, WriteError::ColumnSetMismatch { row: 1 });

    // The control: the caller who meant NULL says so, and it is accepted.
    let accepted = Insert::builder(collection(), Returning::nothing(), BindBudget::POSTGRES)
        .row(vec![cell("a", 1), cell("b", 2)])
        .row(vec![
            cell("a", 3),
            ColumnValue::new(column("b"), WriteValue::Null),
        ])
        .build()
        .expect("an explicit null is a written column");
    let sql = postgres::render_insert(&accepted)
        .expect("renders")
        .sql()
        .to_string();
    assert!(sql.contains("VALUES ($1, $2), ($3, NULL)"), "{sql}");
    println!("ruled on 2 mismatched batches and 1 explicit null");
}

// ---------------------------------------------------------------------------
// Shape refusals
// ---------------------------------------------------------------------------

/// The empty shapes are all SQL syntax errors, and each is refused with its own
/// variant so the message names what is missing.
#[test]
fn the_empty_write_shapes_are_refused() {
    let mut ruled_on = 0_usize;

    assert_eq!(
        Insert::builder(collection(), Returning::nothing(), BindBudget::POSTGRES)
            .build()
            .expect_err("must refuse"),
        WriteError::EmptyBatch
    );
    ruled_on += 1;

    assert_eq!(
        Insert::builder(collection(), Returning::nothing(), BindBudget::POSTGRES)
            .row(vec![])
            .build()
            .expect_err("must refuse"),
        WriteError::EmptyColumnList
    );
    ruled_on += 1;

    assert_eq!(
        Update::builder(collection(), RowLimit::default(), Returning::nothing())
            .build()
            .expect_err("must refuse"),
        WriteError::EmptyAssignments
    );
    ruled_on += 1;

    assert_eq!(ruled_on, 3);
    println!("ruled on {ruled_on} empty write shapes");
}

/// `SET a = 1, a = 2` is refused by `PostgreSQL` outright, and silently keeping
/// one of them would pick a winner by sort stability.
#[test]
fn a_column_written_twice_is_refused() {
    assert_eq!(
        Update::builder(collection(), RowLimit::default(), Returning::nothing())
            .set(ColumnAssignment::new(
                column("name"),
                Assignment::bind(Literal::Int(1))
            ))
            .set(ColumnAssignment::new(
                column("name"),
                Assignment::bind(Literal::Int(2))
            ))
            .build()
            .expect_err("must refuse"),
        WriteError::DuplicateColumn {
            column: "name".to_string()
        }
    );
    assert_eq!(
        Insert::builder(collection(), Returning::nothing(), BindBudget::POSTGRES)
            .row(vec![cell("name", 1), cell("name", 2)])
            .build()
            .expect_err("must refuse"),
        WriteError::DuplicateColumn {
            column: "name".to_string()
        }
    );
    println!("ruled on 2 duplicate columns");
}

/// An aggregate in a write's filter is the same mistake it is in a read's, and
/// it is refused at the same point - construction, not execution.
#[test]
fn an_aggregate_in_a_write_filter_is_refused() {
    let count_gt = Predicate::compare(
        Operand::Aggregate(AggregateRef::count_rows()),
        CompareOp::Gt,
        Operand::Lit(Literal::Int(5)),
    );
    assert_eq!(
        Update::builder(collection(), RowLimit::default(), Returning::nothing())
            .set(ColumnAssignment::new(
                column("name"),
                Assignment::bind(Literal::Int(1))
            ))
            .filter(count_gt.clone())
            .build()
            .expect_err("must refuse"),
        WriteError::AggregateInFilter
    );
    assert_eq!(
        Delete::builder(collection(), RowLimit::default(), Returning::nothing())
            .filter(count_gt)
            .build()
            .expect_err("must refuse"),
        WriteError::AggregateInFilter
    );
    println!("ruled on 2 misplaced aggregates");
}

/// SC-3's decision 2 applies to the write family's own lowering: an unservable
/// node is a typed error from the backend's module, not a guess. A nested JSON
/// path is the subject, in both positions a write can carry one.
#[test]
fn an_unservable_node_in_a_write_is_refused_with_a_typed_error() {
    let nested = || {
        FieldPath::nested(
            column("payload"),
            vec![JsonKey::new("customer").expect("key")],
        )
        .expect("nested path")
    };

    // In the filter.
    let update = Update::builder(collection(), RowLimit::default(), Returning::nothing())
        .set(ColumnAssignment::new(
            column("name"),
            Assignment::bind(Literal::Int(1)),
        ))
        .filter(Predicate::compare(
            Operand::Path(nested()),
            CompareOp::Eq,
            Operand::Lit(Literal::Int(1)),
        ))
        .build()
        .expect("the PLAN is valid; only the lowering refuses");
    let outcome = postgres::render_update(&update);
    assert!(
        matches!(outcome, Err(RenderError::Unsupported { .. })),
        "expected a typed refusal, got {outcome:?}"
    );

    // In the RETURNING list.
    let returning = Returning::rows(
        Projection::rows(vec![ProjectedField {
            source: zeroship_data_query_builder::ProjectionSource::Path(nested()),
            alias: alias("customer"),
            exposure: zeroship_data_query_builder::Exposure::Declared,
        }])
        .expect("row projection"),
    )
    .expect("returning");
    let outcome = postgres::render_insert(&insert_one(returning));
    assert!(
        matches!(outcome, Err(RenderError::Unsupported { .. })),
        "expected a typed refusal, got {outcome:?}"
    );

    // The control: the same shapes without nesting render.
    assert!(postgres::render_insert(&insert_one(returning_name())).is_ok());
    println!("ruled on 2 unservable positions and 1 control");
}
