//! Which dependents make PostgreSQL refuse a bare `ALTER TABLE ... DROP COLUMN`.
//!
//! The planned column-drop dependency gate has to refuse exactly what the server
//! refuses. Too narrow and it lets the drop reach the error it exists to prevent;
//! too wide and it rejects migrations that apply cleanly, which is the failure mode
//! that has already reversed several fixes in this review.
//!
//! So this test does not assert a hand-written list. It builds one column per
//! dependent shape, asks a CATALOG PREDICATE whether each drop should be refused,
//! then actually attempts each drop and asserts the two agree. The predicate is the
//! thing the gate will be built on; the attempt is PostgreSQL's own answer. A server
//! upgrade that changes the behaviour breaks this rather than the gate.
//!
//! The obvious predicate is wrong, which is why this is measured rather than
//! assumed. Filtering `pg_depend` for a NORMAL dependency catches the generated
//! column and the view and MISSES an EXCLUDE whose expression reads the column -
//! that one reports an AUTO dependency and is still refused. The reason is
//! ownership: an exclusion's index is INTERNALLY owned by its constraint, so
//! auto-dropping the index is not a route the server will take on its own. When the
//! constraint ALSO depends on the column directly, that path drops the whole
//! constraint and the column goes - which is why an exclusion naming the column both
//! plainly and inside an expression is droppable while the expression-only form is
//! not.
//!
//! Each drop runs inside a rolled-back transaction, so all shapes are measured
//! against one fixture and none of them consume it.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::{ExecutorConfig, MigrationBackend, PostgresBackend};

/// The shipped predicate's own answer for one column: the blockers it would name, and
/// whether naming any of them means refusal.
///
/// This calls `PostgresBackend::blocking_column_dependents` rather than re-spelling its
/// SQL. An earlier version of this test carried a second copy of the query, which made
/// the agreement it reported an agreement about the COPY: the shipped function's doc
/// comment said so outright, that changing the query here would not fail the oracle. A
/// measurement of code that never runs certifies nothing.
async fn shipped_blockers(
    session: &support::PgDevSession,
    schema: &str,
    column: &str,
) -> Vec<String> {
    let cfg = ExecutorConfig::new(schema.to_string(), schema, support::no_inject(schema));
    PostgresBackend::new_generic(session)
        .blocking_column_dependents(&cfg, "t", column)
        .await
        .unwrap_or_else(|e| panic!("blocking_column_dependents for {column}: {e}"))
}

/// One column per dependent shape, and what the server is expected to do with it.
/// The expectation is asserted against a real drop, so a wrong entry here fails
/// rather than quietly redefining the test.
const SHAPES: &[(&str, bool)] = &[
    ("gen_src", true),     // a generated column reads it
    ("view_src", true),    // a view reads it
    ("excl_expr", true),   // an EXCLUDE expression reads it
    ("gen_out", false),    // the generated column itself
    ("excl_plain", false), // EXCLUDE over the plain column
    ("excl_mixed", false), // EXCLUDE naming it plainly AND in an expression
    ("idx_pred", false),   // a partial index predicate reads it
    ("idx_key", false),    // an index keys on it
    ("idx_expr", false),   // an index expression key reads it
    ("plain", false),      // nothing depends on it
    // An EXCLUDE whose WHERE predicate reads it and whose KEY does not, paired with
    // `idx_pred` - the same predicate shape on a BARE index, which drops cleanly.
    //
    // These add SHAPE coverage, not BRANCH coverage, and the difference was measured
    // rather than assumed: removing the internal-ownership check from the predicate
    // fails on `idx_pred` first, so `excl_expr` and `idx_pred` already exercise both
    // sides of that branch. What no other shape supplies is a column reaching a
    // constraint through an index PREDICATE rather than through an expression KEY.
    // Those are different catalog constructs that happen to produce the same
    // dependency today, so a future predicate that tried to tell them apart would be
    // caught here and nowhere else.
    ("excl_pred", true),
    // The KEY column of that same constraint. Its dependency lands on the CONSTRAINT
    // rather than the index, so PostgreSQL drops the whole constraint and allows it.
    ("excl_pred_key", false),
    // A CHECK constraint, which reports a NORMAL dependency and is STILL droppable -
    // PostgreSQL auto-drops the constraint and takes the column with it. Every shape
    // above either blocks with a NORMAL dependency or allows without one, so a rule
    // keyed on "any NORMAL dependency blocks" agreed with the server twelve times
    // while being wrong. These three are the counterexample, and they are the reason
    // the predicate has to ask whether the SAME dependent also holds an AUTO edge.
    //
    // Both spellings exist because they reach the column differently: a column
    // constraint names one column, a table constraint names several, and dropping any
    // one of the several still takes the whole constraint.
    ("chk_single", false),
    ("chk_multi_a", false),
    ("chk_multi_b", false),
    // Two SEPARATE exclusions on one column: one reading it only through an
    // expression, one naming it plainly. Each is already covered alone - `excl_expr`
    // refuses, `excl_plain` allows - and `excl_mixed` covers both readings inside ONE
    // constraint, which allows. This is the composition none of them reach, and the
    // server refuses it: the plain constraint drops with the column, but the
    // expression-only constraint's index still reads it and nothing may drop that
    // index on its own.
    //
    // It is the shape that shows the difference between "some constraint on this
    // column also depends on it directly" and "the constraint OWNING THE BLOCKING
    // INDEX also depends on it directly". Only the second is the question worth
    // asking, and `excl_mixed` cannot tell them apart because there the two are the
    // same constraint.
    ("excl_sep", true),
];

#[compio::test]
async fn the_catalog_predicate_agrees_with_postgres_about_every_blocked_column_drop() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("zm_dep_oracle_{}", std::process::id());
    // The fixture outlives an assertion failure without this: a panic unwinds past
    // the explicit drop below and leaves the schema in the shared test database, one
    // per process id, until someone notices. Five of them were sitting in the local
    // container when this was armed.
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    session
        .batch(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .await
        .expect("clear any leftover fixture");
    session
        .batch(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.t (
               gen_src int, gen_out int GENERATED ALWAYS AS (gen_src * 2) STORED,
               view_src int,
               excl_expr text,
               excl_plain int4range,
               excl_mixed text,
               idx_pred int, idx_key int, idx_expr int,
               excl_pred text, excl_pred_key text,
               chk_single int CHECK (chk_single > 0),
               chk_multi_a int, chk_multi_b int,
               CONSTRAINT c_multi CHECK (chk_multi_a > 0 AND chk_multi_b > 0),
               excl_sep text,
               plain int
             );
             CREATE VIEW {schema}.v AS SELECT view_src FROM {schema}.t;
             ALTER TABLE {schema}.t ADD CONSTRAINT c_expr
               EXCLUDE USING btree (lower(excl_expr) WITH =);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_plain
               EXCLUDE USING gist (excl_plain WITH &&);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_mixed
               EXCLUDE USING btree (excl_mixed WITH =, lower(excl_mixed) WITH =);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_pred
               EXCLUDE USING btree (excl_pred_key WITH =) WHERE (excl_pred IS NOT NULL);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_sep_expr
               EXCLUDE USING btree (lower(excl_sep) WITH =);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_sep_plain
               EXCLUDE USING btree (excl_sep WITH =);
             CREATE INDEX i_pred ON {schema}.t (idx_key) WHERE (idx_pred > 0);
             CREATE INDEX i_expr ON {schema}.t ((idx_expr + 1))"
        ))
        .await
        .expect("build the one-column-per-shape fixture");

    let table = format!("{schema}.t");
    let mut checked = 0usize;
    for (column, expected_refusal) in SHAPES {
        // What the shipped gate says, by running it.
        let blockers = shipped_blockers(&session, &schema, column).await;
        let predicted = !blockers.is_empty();

        // What PostgreSQL actually does. Rolled back so the next shape still has its
        // fixture, and so a column that DOES drop cannot remove a later dependent.
        session.batch("BEGIN").await.expect("open probe txn");
        let attempted = session
            .batch(&format!("ALTER TABLE {table} DROP COLUMN {column}"))
            .await;
        session.batch("ROLLBACK").await.expect("close probe txn");
        let refused = attempted.is_err();

        assert_eq!(
            refused, *expected_refusal,
            "PostgreSQL changed its mind about dropping a column with this dependent \
             ({column}); the gate's rule is derived from this behaviour, so re-measure \
             before trusting it"
        );
        assert_eq!(
            predicted,
            refused,
            "blocking_column_dependents disagrees with the server for {column}: it named \
             {blockers:?}, actual refuse={refused}. The shipped gate would {} here",
            if predicted {
                "reject a migration PostgreSQL accepts"
            } else {
                "wave through a drop PostgreSQL rejects"
            }
        );
        checked += 1;
    }

    assert_eq!(
        checked,
        SHAPES.len(),
        "every shape was compared; a loop that skipped one would prove nothing about it"
    );

    session
        .batch(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .expect("drop the fixture");
}
