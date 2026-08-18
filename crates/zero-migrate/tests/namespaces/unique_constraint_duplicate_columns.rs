//! A UNIQUE constraint naming the same column twice is refused at the gate.
//!
//! Measured before the fix. `UNIQUE (c, c)` passed `validate_ir`, passed the load
//! gate, and lowered to
//!
//!     ALTER TABLE "prj_ir"."a" ADD CONSTRAINT "a_c_c_key" UNIQUE (c, c)
//!
//! which PostgreSQL rejects at APPLY:
//!
//!     ERROR: column "c" appears twice in unique constraint
//!
//! The engine ALREADY refuses exactly this shape elsewhere. A foreign key whose
//! local columns repeat is rejected with "local column \"c\" appears more than
//! once", and a `primaryKey` that repeats a column is rejected too. UNIQUE was
//! the one constraint kind of the three without the check, so the same authoring
//! mistake was caught at the gate or at the server depending only on which
//! constraint you wrote it in.
//!
//! CONTRAST WITH INDEXES, which is why this is scoped to constraints. PostgreSQL
//! ACCEPTS `CREATE INDEX ON a (c, c)` — verified against the server — so a
//! duplicate column in an index is legal, merely wasteful, and refusing it would
//! reject a migration the database is happy to run. Only the constraint form is
//! an error.

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(op: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{op}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
}

/// Assert WHICH rule refused, so the three kinds cannot cover for each other.
///
/// The last test here deliberately exercises three DIFFERENT rules in one body -
/// unique, foreign key and primary key - to pin that the same authoring mistake
/// is refused whichever constraint carries it. Bare expect_err made that test
/// pass if ANY one of the three fired, which is the opposite of what it claims.
fn expect_refusal_from(ops: &str, needle: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        refusal.contains(needle),
        "{needle:?} is missing, so another constraint kind is satisfying this \
         test: {refusal}"
    );
}

#[test]
fn add_constraint_unique_repeating_a_column_is_refused() {
    let refusal = verdict(
        r#"{"op":"addConstraint","table":"a","constraint":{"kind":{"kind":"unique","columns":["c","c"]}}}"#,
    )
    .expect_err(
        "UNIQUE (c, c) lowers to SQL PostgreSQL rejects with `column \"c\" appears \
         twice in unique constraint`, so the operator meets it during apply instead \
         of at authoring time",
    );
    assert!(
        refusal.to_lowercase().contains("more than once")
            || refusal.to_lowercase().contains("twice")
            || refusal.to_lowercase().contains("duplicate"),
        "the refusal must name the repetition as the problem: {refusal}"
    );
}

#[test]
fn create_table_with_an_inline_unique_repeating_a_column_is_refused() {
    // The same mistake authored in the other place it can be written.
    expect_refusal_from(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false}],"primaryKey":["c"],"constraints":[{"kind":{"kind":"unique","columns":["c","c"]}}]}"#,
        "unique constraint",
        "an inline UNIQUE repeating a column must be refused like the standalone form",
    );
}

#[test]
fn a_unique_over_distinct_columns_is_still_allowed() {
    // The control. Refusing every UNIQUE would satisfy both tests above.
    verdict(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"d","type":"int","nullable":true}],"primaryKey":["c"],"constraints":[{"kind":{"kind":"unique","columns":["c","d"]}}]}"#,
    )
    .expect("a UNIQUE over two distinct columns is ordinary and must pass");
}

#[test]
fn the_sibling_constraint_kinds_already_refuse_it() {
    // Pins the consistency this fix restores rather than assuming it: the same
    // authoring mistake must be refused whichever constraint kind carries it.
    expect_refusal_from(
        r#"{"op":"addConstraint","table":"a","constraint":{"kind":{"kind":"fk","columns":["c","c"],"referencesTable":"b","referencesColumns":["x","y"]}}}"#,
        "foreign key",
        "a foreign key repeating a local column is refused",
    );

    expect_refusal_from(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false}],"primaryKey":["c","c"]}"#,
        "primaryKey names column",
        "a primaryKey repeating a column is refused",
    );
}
