//! Two semantically identical plans built by OPPOSITE insertion permutations
//! render to one canonical SQL/parameter fixture.
//!
//! This is SC-3's current specification of the determinism arm. The superseded
//! one - render the same plan twice, assert the SQL matches - is deliberately
//! NOT implemented: rendering one in-memory unordered map twice can preserve
//! that instance's iteration order, so a non-canonical implementation passes
//! it. It is probabilistic, not discriminating.
//!
//! The mutation half of the arm - delete the canonical sort, prove this turns
//! red - is `permuted_conjuncts_diverge_without_the_canonical_sort` in
//! `src/render/postgres.rs`. It has to live there because it calls the private
//! writer directly, which is the only way to render an un-canonicalised
//! predicate without exposing a public way to do so. A public
//! "render without canonicalising" knob would be the hole this crate exists to
//! avoid.
//!
//! Why any of it matters: the driver's prepared-statement cache keys on
//! statement text. A rendering that iterated an unordered map would pass every
//! correctness test in this repository and miss the cache on every call.

use zeroship_data_orm::sql::render::postgres;
use zeroship_data_orm::sql::{
    ArithmeticOp, Assignment, BindBudget, ColumnAssignment, ColumnValue, CompareOp, Direction,
    Ident, IdentRole, Insert, Literal, MembershipOp, NullOrder, Operand, OrderKey, Predicate,
    ProjectedField, Projection, Returning, RowLimit, Select, Update, WriteValue,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn eq(name: &str, value: i64) -> Predicate {
    Predicate::compare(
        Operand::column(column(name)),
        CompareOp::Eq,
        Operand::Lit(Literal::Int(value)),
    )
}

fn field(name: &str) -> ProjectedField {
    ProjectedField::column(column(name)).expect("projectable")
}

/// The single canonical fixture both permutations must produce.
///
/// It is written out in full rather than compared permutation-to-permutation,
/// because two wrong renderings agree with each other perfectly.
/// It carried seven more columns until 2026-09-07, when the projection stopped
/// unioning a platform-field list of its own. Canonical ORDER is what this
/// fixture exists to pin, and that is unchanged: the two permutations still
/// sort to one statement. What changed is only which columns a caller has to
/// name for themselves.
const CANONICAL_SQL: &str = concat!(
    r#"SELECT "email" AS "email", "name" AS "name" "#,
    r#"FROM "app_1"."users" "#,
    r#"WHERE ("age" = $1 AND "score" = $2) "#,
    r#"ORDER BY "name" ASC NULLS LAST "#,
    r#"LIMIT $3 OFFSET $4"#
);

fn plan(projection_order: [&str; 2], conjunct_order: [(&str, i64); 2]) -> Select {
    let projection = Projection::rows(projection_order.iter().map(|n| field(n)).collect())
        .expect("row projection");
    Select::builder(
        Ident::parse_as("users", IdentRole::Collection).expect("collection"),
        projection,
    )
    .namespace(Ident::parse_as("app_1", IdentRole::Namespace).expect("namespace"))
    .filter(
        Predicate::and(conjunct_order.iter().map(|(n, v)| eq(n, *v)).collect())
            .expect("within the depth bound"),
    )
    .order_by(vec![OrderKey {
        path: zeroship_data_orm::sql::FieldPath::column(column("name")),
        direction: Direction::Ascending,
        nulls: NullOrder::Last,
    }])
    .limit(RowLimit::new(25).expect("limit"))
    .build()
    .expect("valid plan")
}

#[test]
fn opposite_insertion_permutations_render_to_one_canonical_fixture() {
    let forwards = plan(["name", "email"], [("age", 30), ("score", 90)]);
    let backwards = plan(["email", "name"], [("score", 90), ("age", 30)]);

    let a = postgres::render_select(&forwards).expect("renders");
    let b = postgres::render_select(&backwards).expect("renders");

    assert_eq!(a.sql(), CANONICAL_SQL, "forward permutation drifted");
    assert_eq!(b.sql(), CANONICAL_SQL, "reverse permutation drifted");
    assert_eq!(
        a.params(),
        &[
            Literal::Int(30),
            Literal::Int(90),
            Literal::Int(25),
            Literal::Int(0)
        ],
        "parameters must follow the canonical statement order, not the authored one"
    );
    assert_eq!(a.params(), b.params());
    println!("ruled on 2 permutations against 1 fixture");
}

/// A different PATH to the same plan, not merely a different order: one caller
/// nests the conjunction and repeats a conjunct, the other writes it flat.
/// Associativity, commutativity and idempotence all have to normalise away.
#[test]
fn plans_built_by_different_paths_produce_one_statement() {
    let nested = Predicate::and(vec![
        Predicate::and(vec![eq("score", 90), eq("age", 30)]).expect("shallow"),
        eq("age", 30),
        Predicate::always(),
    ])
    .expect("shallow");
    let flat = Predicate::and(vec![eq("age", 30), eq("score", 90)]).expect("shallow");
    assert_eq!(nested, flat, "the two authoring paths did not converge");

    let projection = Projection::rows(vec![field("name")]).expect("row projection");
    let build = |filter: Predicate| {
        Select::builder(
            Ident::parse_as("users", IdentRole::Collection).expect("collection"),
            projection.clone(),
        )
        .filter(filter)
        .build()
        .expect("valid plan")
    };
    let a = postgres::render_select(&build(nested)).expect("renders");
    let b = postgres::render_select(&build(flat)).expect("renders");
    assert_eq!(a.sql(), b.sql());
    assert_eq!(a.params(), b.params());
    println!("ruled on 2 authoring paths");
}

/// Membership sets are a second unordered collection, and they carry
/// parameters - so a non-canonical set would change both the statement and the
/// binding order.
#[test]
fn permuted_membership_sets_render_identically() {
    let forwards = Predicate::membership(
        Operand::column(column("status")),
        MembershipOp::In,
        vec![
            Some(Literal::Int(3)),
            Some(Literal::Int(1)),
            Some(Literal::Int(2)),
        ],
    )
    .expect("membership");
    let backwards = Predicate::membership(
        Operand::column(column("status")),
        MembershipOp::In,
        vec![
            Some(Literal::Int(2)),
            Some(Literal::Int(3)),
            Some(Literal::Int(1)),
            // A duplicate: a set test does not care, and the canonical form
            // must not either.
            Some(Literal::Int(3)),
        ],
    )
    .expect("membership");
    assert_eq!(forwards, backwards);

    let projection = Projection::rows(vec![field("name")]).expect("row projection");
    let build = |filter: Predicate| {
        Select::builder(
            Ident::parse_as("orders", IdentRole::Collection).expect("collection"),
            projection.clone(),
        )
        .filter(filter)
        .build()
        .expect("valid plan")
    };
    let a = postgres::render_select(&build(forwards)).expect("renders");
    let b = postgres::render_select(&build(backwards)).expect("renders");
    assert_eq!(a.sql(), b.sql());
    assert!(
        a.sql().contains(r#""status" IN ($1, $2, $3)"#),
        "unexpected membership lowering: {}",
        a.sql()
    );
    assert_eq!(
        a.params()[..3],
        [Literal::Int(1), Literal::Int(2), Literal::Int(3)]
    );
    println!("ruled on 2 permutations of 1 membership set");
}

// ---------------------------------------------------------------------------
// The write family. Same specification, same mutation discipline: the
// divergence arms live in `src/render/postgres.rs`, because they call the
// private writers directly, which is the only way to render an
// un-canonicalised column or assignment list without exposing a public way to
// do so.
// ---------------------------------------------------------------------------

/// The `RETURNING` list both write fixtures below produce: the two declared
/// columns, in canonical alias order.
const RETURNING_LIST: &str = r#" RETURNING "email" AS "email", "name" AS "name""#;

fn write_returning(order: [&str; 2]) -> Returning {
    Returning::rows(
        Projection::rows(order.iter().map(|n| field(n)).collect()).expect("row projection"),
    )
    .expect("returning")
}

fn namespace() -> Ident {
    Ident::parse_as("app_1", IdentRole::Namespace).expect("namespace")
}

fn users() -> Ident {
    Ident::parse_as("users", IdentRole::Collection).expect("collection")
}

/// The column list of an INSERT is an unordered set from the caller's side - a
/// document's keys - so two callers who wrote the same document with the keys
/// in different orders must produce one statement AND one binding order. The
/// second half is what makes this different from the projection case: the
/// values follow the columns, so a non-canonical column list also permutes the
/// parameters.
#[test]
fn permuted_insert_columns_render_to_one_canonical_fixture() {
    let canonical =
        format!(r#"INSERT INTO "app_1"."users" ("email", "name") VALUES ($1, $2){RETURNING_LIST}"#);

    let build = |cells: [(&str, i64); 2], returning: [&str; 2]| {
        Insert::builder(users(), write_returning(returning), BindBudget::POSTGRES)
            .namespace(namespace())
            .row(
                cells
                    .iter()
                    .map(|(n, v)| ColumnValue::new(column(n), WriteValue::Bind(Literal::Int(*v))))
                    .collect(),
            )
            .build()
            .expect("valid insert")
    };

    let forwards = build([("name", 1), ("email", 2)], ["name", "email"]);
    let backwards = build([("email", 2), ("name", 1)], ["email", "name"]);

    let a = postgres::render_insert(&forwards).expect("renders");
    let b = postgres::render_insert(&backwards).expect("renders");
    assert_eq!(a.sql(), canonical, "forward permutation drifted");
    assert_eq!(b.sql(), canonical, "reverse permutation drifted");
    assert_eq!(
        a.params(),
        &[Literal::Int(2), Literal::Int(1)],
        "parameters must follow the canonical column order, not the authored one"
    );
    assert_eq!(a.params(), b.params());
    println!("ruled on 2 permutations against 1 fixture");
}

/// The `SET` list is likewise unordered: every assignment in one `UPDATE` reads
/// the row as it was before the statement, so the clauses do not chain and the
/// authored order is not observable.
#[test]
fn permuted_update_assignments_render_to_one_canonical_fixture() {
    let canonical = format!(
        concat!(
            r#"UPDATE "app_1"."users" SET "name" = $1, "version" = "version" + $2 "#,
            r#"WHERE "id" IN (SELECT "id" FROM "app_1"."users" "#,
            r#"WHERE ("age" = $3 AND "score" = $4) LIMIT $5 FOR UPDATE){}"#
        ),
        RETURNING_LIST
    );

    let name = || ColumnAssignment::new(column("name"), Assignment::bind(Literal::Int(7)));
    let version = || {
        ColumnAssignment::new(
            column("version"),
            Assignment::arithmetic(ArithmeticOp::Add, Literal::Int(1)).expect("numeric"),
        )
    };

    let build = |sets: [ColumnAssignment; 2], conjuncts: [(&str, i64); 2], returning: [&str; 2]| {
        let mut builder = Update::builder(
            users(),
            zeroship_data_orm::sql::Ident::parse_as("id", zeroship_data_orm::sql::IdentRole::Column).unwrap(),
            RowLimit::new(25).expect("limit"),
            write_returning(returning),
        )
        .namespace(namespace())
        .filter(
            Predicate::and(conjuncts.iter().map(|(n, v)| eq(n, *v)).collect())
                .expect("within the depth bound"),
        );
        for assignment in sets {
            builder = builder.set(assignment);
        }
        builder.build().expect("valid update")
    };

    let forwards = build(
        [name(), version()],
        [("age", 30), ("score", 90)],
        ["name", "email"],
    );
    let backwards = build(
        [version(), name()],
        [("score", 90), ("age", 30)],
        ["email", "name"],
    );

    let a = postgres::render_update(&forwards).expect("renders");
    let b = postgres::render_update(&backwards).expect("renders");
    assert_eq!(a.sql(), canonical, "forward permutation drifted");
    assert_eq!(b.sql(), canonical, "reverse permutation drifted");
    assert_eq!(
        a.params(),
        &[
            Literal::Int(7),
            Literal::Int(1),
            Literal::Int(30),
            Literal::Int(90),
            Literal::Int(25),
        ],
        "parameters must follow the canonical statement order"
    );
    assert_eq!(a.params(), b.params());
    println!("ruled on 2 permutations against 1 fixture");
}

/// The ROWS of an insert are the one write list a canonicaliser must NOT touch,
/// and the reason is the same one that protects `ORDER BY`: the order is
/// observable. `RETURNING` yields the rows in insertion order, so reordering
/// them reorders the result the caller stitches ids back onto.
#[test]
fn insert_rows_keep_their_authored_order() {
    let build = |rows: [i64; 2]| {
        let mut builder = Insert::builder(users(), Returning::nothing(), BindBudget::POSTGRES);
        for value in rows {
            builder = builder.row(vec![ColumnValue::new(
                column("n"),
                WriteValue::Bind(Literal::Int(value)),
            )]);
        }
        postgres::render_insert(&builder.build().expect("valid insert"))
            .expect("renders")
            .params()
            .to_vec()
    };
    let ab = build([1, 2]);
    let ba = build([2, 1]);
    assert_ne!(
        ab, ba,
        "the row order was canonicalised, which changes which row RETURNING yields first"
    );
    assert_eq!(ab, vec![Literal::Int(1), Literal::Int(2)]);
    println!("ruled on 2 row permutations");
}

/// `ORDER BY` is the one list a canonicaliser must NOT touch: `ORDER BY a, b`
/// and `ORDER BY b, a` are different queries. This arm exists so a future
/// "sort everything" simplification fails loudly rather than silently changing
/// what page a caller gets.
#[test]
fn order_by_keys_keep_their_authored_order() {
    let key = |name: &str| OrderKey {
        path: zeroship_data_orm::sql::FieldPath::column(column(name)),
        direction: Direction::Ascending,
        nulls: NullOrder::Last,
    };
    let projection = Projection::rows(vec![field("name")]).expect("row projection");
    let build = |keys: Vec<OrderKey>| {
        let rendered = postgres::render_select(
            &Select::builder(
                Ident::parse_as("users", IdentRole::Collection).expect("collection"),
                projection.clone(),
            )
            .order_by(keys)
            .build()
            .expect("valid plan"),
        )
        .expect("renders");
        rendered.sql().to_string()
    };
    let ab = build(vec![key("alpha"), key("beta")]);
    let ba = build(vec![key("beta"), key("alpha")]);
    assert_ne!(
        ab, ba,
        "ORDER BY was canonicalised, which changes which rows a page contains"
    );
    assert!(ab.contains(r#"ORDER BY "alpha" ASC NULLS LAST, "beta" ASC NULLS LAST"#));
    println!("ruled on 2 order-key permutations");
}
