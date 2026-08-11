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

/// Every user column of the fixture table, paired with the catalog footprint the
/// shipped predicate reads for it.
///
/// This is the SECOND enumeration basis. The shape list below is a set of constructs
/// somebody thought of, so it cannot say anything about a construct nobody thought
/// of - which is exactly how a wrong rule collected fifteen agreements twice. Here
/// the server supplies both the columns and the grouping key, so which columns are
/// COMPARABLE is not a judgement anyone makes.
///
/// The footprint is the predicate's own input basis, no more and no less: per
/// dependency row, `deptype`, `classid`, whether that object is an index a constraint
/// internally owns, and whether THAT OWNING CONSTRAINT also depends on the column
/// directly. `objid` appears only as a dense rank, because the query asks whether the
/// SAME object holds another edge and never reads the oid as a value - a raw oid would
/// make every column unique and the grouping vacuous. `objsubid` is excluded: it feeds
/// `pg_describe_object` for the blocker TEXT and takes no part in the refuse decision.
///
/// The owner term is here because leaving it out made this test FAIL, which is the
/// useful part of writing it. Without it `excl_mixed` and `excl_sep` share a footprint
/// - one constraint reading the column two ways, versus two constraints reading it one
/// way each - and PostgreSQL treats them differently. That reads as "no predicate over
/// this basis can be correct", and it was wrong: the shipped query separates them
/// perfectly by following the internal edge's `refobjid`. The footprint was modelled
/// on the query as it stood BEFORE that leg existed. A grouping key coarser than the
/// real basis manufactures impossibility; one finer than it merely proves less.
///
/// Restricted to `t` rather than every relation in the schema. The view and the
/// indexes have `pg_attribute` rows too, and a failed drop on one of those reports a
/// relation kind rather than a dependency, which would look like disagreement.
fn signature_sql(table: &str) -> String {
    format!(
        "WITH cols AS (
   SELECT att.attname, att.attnum, att.attrelid
     FROM pg_attribute att
    WHERE att.attrelid = '{table}'::regclass
      AND att.attnum > 0 AND NOT att.attisdropped
), dep AS (
   SELECT c.attname, d.deptype::text AS deptype, d.classid, d.objid,
          EXISTS (SELECT 1 FROM pg_depend i
                   WHERE i.classid = 'pg_class'::regclass AND i.objid = d.objid
                     AND i.deptype = 'i'
                     AND i.refclassid = 'pg_constraint'::regclass) AS constraint_owned,
          EXISTS (SELECT 1 FROM pg_depend i
                   WHERE i.classid = 'pg_class'::regclass AND i.objid = d.objid
                     AND i.deptype = 'i'
                     AND i.refclassid = 'pg_constraint'::regclass
                     AND EXISTS (SELECT 1 FROM pg_depend o
                                  WHERE o.refobjid = c.attrelid
                                    AND o.refobjsubid = c.attnum
                                    AND o.refclassid = 'pg_class'::regclass
                                    AND o.deptype = 'a'
                                    AND o.classid = 'pg_constraint'::regclass
                                    AND o.objid = i.refobjid)) AS owner_depends
     FROM cols c
     LEFT JOIN pg_depend d ON d.refobjid = c.attrelid AND d.refobjsubid = c.attnum
                          AND d.refclassid = 'pg_class'::regclass
), tup AS (
   SELECT attname,
          coalesce(deptype, '-') || ':'
            || coalesce(classid::regclass::text, '-') || ':'
            || coalesce(constraint_owned::text, '-') || ':'
            || coalesce(owner_depends::text, '-') || ':#'
            || (dense_rank() OVER (PARTITION BY attname ORDER BY objid))::text AS t
     FROM dep
)
SELECT attname, string_agg(DISTINCT t, ' | ' ORDER BY t) AS signature
  FROM tup GROUP BY attname ORDER BY attname"
    )
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

    // The second basis, read before the drop probes so the fixture is intact.
    let signature_rows = session
        .query(&signature_sql(&table), &[])
        .await
        .expect("read the catalog footprint of every fixture column");
    let signatures: std::collections::BTreeMap<String, String> = signature_rows
        .iter()
        .map(|row| {
            (
                row.try_get::<_, String>("attname").expect("decode attname"),
                row.try_get::<_, String>("signature")
                    .expect("decode signature"),
            )
        })
        .collect();

    // The fixture and the shape list are two hand-written things that can drift
    // apart. A column added to the DDL and forgotten here would be checked by
    // nothing, and the loop below would still report every shape compared.
    let enumerated: std::collections::BTreeSet<&str> =
        signatures.keys().map(String::as_str).collect();
    let listed: std::collections::BTreeSet<&str> = SHAPES.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        enumerated, listed,
        "the fixture table's columns and the SHAPES list have drifted apart; a column \
         the server reports but the list omits is covered by nothing"
    );

    let mut checked = 0usize;
    let mut by_signature: std::collections::BTreeMap<String, Vec<(&str, bool)>> =
        std::collections::BTreeMap::new();
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
        by_signature
            .entry(signatures[*column].clone())
            .or_default()
            .push((column, refused));
        checked += 1;
    }

    // Does the footprint the predicate reads DETERMINE the server's answer? Every
    // check above asks whether the predicate got a known column right. This asks
    // something the shape list cannot: whether a predicate over this input basis
    // could be right at all. Two columns the catalog describes identically that
    // PostgreSQL treats differently would mean no rule reading only this basis can
    // agree with the server everywhere, and the repair would be to widen what the
    // query reads rather than to adjust its logic.
    let mut compared_groups = 0usize;
    for (signature, members) in &by_signature {
        if members.len() < 2 {
            continue;
        }
        compared_groups += 1;
        let (first_column, first_answer) = members[0];
        for (column, answer) in &members[1..] {
            assert_eq!(
                *answer, first_answer,
                "{first_column} and {column} have the same catalog footprint \
                 ({signature}) and PostgreSQL disagrees about them: refuse={first_answer} \
                 versus refuse={answer}. The predicate reads only this footprint, so no \
                 rule over it can be right about both; the query has to read something \
                 it currently ignores"
            );
        }
    }
    assert!(
        compared_groups >= 2,
        "every column has a footprint of its own, so the comparison above compared \
         nothing; the grouping key has become finer than the predicate's real input \
         basis, or the fixture lost the columns that used to share one"
    );

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
