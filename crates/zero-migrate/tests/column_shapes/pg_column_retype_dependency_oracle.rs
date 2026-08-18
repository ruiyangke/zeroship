//! Which companions make PostgreSQL refuse an `ALTER TABLE ... ALTER COLUMN ... TYPE`.
//!
//! The sibling of `pg_column_drop_dependency_oracle.rs`, and it exists because the
//! two questions have DIFFERENT answers. That was measured, not assumed: run the
//! drop predicate beside the server's retype verdict over one table per companion
//! and seven of eight agree, which is exactly the trap. The eighth is a column
//! another table's FOREIGN KEY points at - a blocker for a drop, not a blocker for
//! a retype - so wiring the existing assertion onto `setColumnType` would refuse a
//! migration PostgreSQL honours. Over-refusal is a defect too.
//!
//! And the disagreement is not one-sided. A retype is refused for two reasons that
//! are not dependencies at all and that no `pg_depend` walk can see: the column
//! being part of the table's PARTITION KEY, and the column being INHERITED from a
//! parent table. A dependency-only predicate under-refuses on both, and the plan
//! half-applies.
//!
//! So this asserts no hand-written list. It builds one column per shape, asks the
//! SHIPPED predicate (`PostgresBackend::column_type_change_blockers`, executed, not
//! re-spelled here) whether the retype should be refused, then actually attempts
//! the retype and asserts the two agree. A server upgrade that changes the
//! behaviour breaks this rather than the gate.
//!
//! **The accepted shapes carry as much weight as the refused ones.** Twelve of the
//! shapes below are companions PostgreSQL is happy to rebuild or revalidate around
//! a retype, and every one of them is a control against the predicate creeping
//! wider. Deleting them would leave "refuse everything" passing.

use crate::support;

use zero_migrate::driver::SqlSession;
use zero_migrate::{ExecutorConfig, MigrationBackend, PostgresBackend};

/// The shipped predicate's own answer for one column.
///
/// This CALLS `PostgresBackend::column_type_change_blockers` rather than
/// re-spelling its SQL. The drop oracle learned that the hard way: it used to carry
/// a second copy of the query, which made the agreement it reported an agreement
/// about the copy.
async fn shipped_blockers(
    session: &support::PgDevSession,
    schema: &str,
    table: &str,
    column: &str,
) -> Vec<String> {
    let cfg = ExecutorConfig::new(schema.to_string(), schema, support::no_inject(schema));
    PostgresBackend::new_generic(session)
        .column_type_change_blockers(&cfg, table, column)
        .await
        .unwrap_or_else(|e| panic!("column_type_change_blockers for {table}.{column}: {e}"))
}

/// One shape per row: `(table, column, target type, expected refusal)`.
///
/// The target type differs by column because the probe must isolate the COMPANION
/// as the only variable. A target the source cannot cast to at all produces
/// "cannot cast type X to type Y", which is a refusal about the types and would
/// read here as a companion that blocks. Each row therefore names a target the
/// bare column would accept.
///
/// `expected_refusal` is asserted against a REAL retype, so a wrong entry fails
/// rather than quietly redefining the test.
const SHAPES: &[(&str, &str, &str, bool)] = &[
    // ---- refused: a dependent object PostgreSQL will not rebuild ----
    ("t", "gen_src", "bigint", true),  // a generated column reads it
    ("t", "view_src", "bigint", true), // a view reads it
    ("t", "matview_src", "bigint", true), // a materialized view reads it
    ("t", "policy_src", "bigint", true), // an RLS policy reads it
    ("t", "trig_col", "bigint", true), // a trigger's UPDATE OF names it
    // ---- refused: not a dependency at all ----
    // Neither of these leaves a blocking `pg_depend` edge. They are refused by
    // `ATPrepAlterColumnType` before any dependency is consulted, and they are the
    // reason this predicate reads two catalogs the drop predicate never opens.
    ("part_plain", "k", "bigint", true), // a plain partition-key column
    ("part_expr", "e", "bigint", true),  // a partition key that is an EXPRESSION
    ("part_child", "k", "bigint", true), // inherited from the partitioned parent
    ("inh_child", "b", "bigint", true),  // inherited via plain INHERITS
    // ---- accepted: the controls ----
    // THE ROW THAT DECIDES THE PREDICATE. Blocks a DROP, does not block a retype.
    ("t", "fk_target", "bigint", false), // another table's FK points AT it
    ("t", "fk_source", "bigint", false), // it points at another table
    ("t", "chk_single", "bigint", false), // a column CHECK
    ("t", "chk_multi_a", "bigint", false), // a table CHECK over several columns
    ("t", "uniq_col", "bigint", false),  // a UNIQUE constraint
    ("t", "idx_key", "bigint", false),   // an index keys on it
    ("t", "idx_expr", "bigint", false),  // an index EXPRESSION reads it
    ("t", "idx_pred", "bigint", false),  // a partial index predicate reads it
    ("t", "def_castable", "bigint", false), // it has a DEFAULT that casts
    ("t", "stats_a", "bigint", false),   // extended statistics span it
    ("t", "seq_owner", "bigint", false), // a sequence is OWNED BY it
    ("t", "plain", "bigint", false),     // nothing at all
    // An EXCLUDE whose EXPRESSION reads the column. This is the shape the drop
    // predicate's whole second leg exists for - it BLOCKS a drop - and PostgreSQL
    // rebuilds the index for a retype and accepts it. Carrying the drop predicate
    // over verbatim fails here as well as on the FK target.
    ("t", "excl_expr", "varchar(64)", false),
    // The same for an EXCLUDE whose WHERE predicate reads it, and for two separate
    // exclusions reading one column two ways (`excl_sep` in the drop oracle, where
    // it refuses).
    ("t", "excl_pred", "varchar(64)", false),
    ("t", "excl_sep", "varchar(64)", false),
    // The generated column ITSELF, as opposed to the column it reads. Accepted -
    // but only because the renderer emits no `USING` for it. Kept here as the
    // counterpart to `gen_src`: they differ by one attribute and the server treats
    // them oppositely.
    ("t", "gen_out", "bigint", false),
];

#[compio::test]
async fn the_catalog_predicate_agrees_with_postgres_about_every_blocked_column_retype() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("zm_retype_oracle_{}", std::process::id());
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
               matview_src int,
               policy_src int,
               trig_col int,
               fk_target int, fk_source int,
               chk_single int CHECK (chk_single > 0),
               chk_multi_a int, chk_multi_b int,
               CONSTRAINT c_multi CHECK (chk_multi_a > 0 AND chk_multi_b > 0),
               uniq_col int,
               idx_key int, idx_expr int, idx_pred int,
               def_castable int DEFAULT 0,
               stats_a int, stats_b int,
               seq_owner int,
               excl_expr text, excl_pred text, excl_pred_key text, excl_sep text,
               plain int,
               CONSTRAINT uq_fk UNIQUE (fk_target),
               CONSTRAINT uq_col UNIQUE (uniq_col)
             );
             CREATE VIEW {schema}.v AS SELECT view_src FROM {schema}.t;
             CREATE MATERIALIZED VIEW {schema}.mv AS SELECT matview_src FROM {schema}.t;
             ALTER TABLE {schema}.t ENABLE ROW LEVEL SECURITY;
             CREATE POLICY p_src ON {schema}.t USING (policy_src > 0);
             CREATE FUNCTION {schema}.trig_fn() RETURNS trigger LANGUAGE plpgsql
               AS $$ BEGIN RETURN NEW; END $$;
             CREATE TRIGGER tg AFTER UPDATE OF trig_col ON {schema}.t
               FOR EACH ROW EXECUTE FUNCTION {schema}.trig_fn();
             CREATE TABLE {schema}.parent (k int PRIMARY KEY);
             ALTER TABLE {schema}.t ADD CONSTRAINT fk_out
               FOREIGN KEY (fk_source) REFERENCES {schema}.parent (k);
             CREATE TABLE {schema}.child (r int,
               CONSTRAINT fk_r FOREIGN KEY (r) REFERENCES {schema}.t (fk_target));
             CREATE INDEX i_key ON {schema}.t (idx_key);
             CREATE INDEX i_expr ON {schema}.t ((idx_expr + 1));
             CREATE INDEX i_pred ON {schema}.t (idx_key) WHERE (idx_pred > 0);
             CREATE STATISTICS {schema}.st ON stats_a, stats_b FROM {schema}.t;
             CREATE SEQUENCE {schema}.sq OWNED BY {schema}.t.seq_owner;
             ALTER TABLE {schema}.t ADD CONSTRAINT c_expr
               EXCLUDE USING btree (lower(excl_expr) WITH =);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_pred
               EXCLUDE USING btree (excl_pred_key WITH =) WHERE (excl_pred IS NOT NULL);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_sep_expr
               EXCLUDE USING btree (lower(excl_sep) WITH =);
             ALTER TABLE {schema}.t ADD CONSTRAINT c_sep_plain
               EXCLUDE USING btree (excl_sep WITH =);
             CREATE TABLE {schema}.part_plain (k int, other int) PARTITION BY RANGE (k);
             CREATE TABLE {schema}.part_child PARTITION OF {schema}.part_plain
               FOR VALUES FROM (0) TO (100);
             CREATE TABLE {schema}.part_expr (e int, other int) PARTITION BY RANGE ((e + 1));
             CREATE TABLE {schema}.inh_base (b int);
             CREATE TABLE {schema}.inh_child (extra int) INHERITS ({schema}.inh_base)"
        ))
        .await
        .expect("build the one-column-per-shape fixture");

    let mut checked = 0usize;
    let mut refused_shapes = 0usize;
    let mut accepted_shapes = 0usize;
    for (table, column, target, expected_refusal) in SHAPES {
        // What the shipped gate says, by running it.
        let blockers = shipped_blockers(&session, &schema, table, column).await;
        let predicted = !blockers.is_empty();

        // What PostgreSQL actually does. Rolled back so the next shape still has its
        // fixture, and so a retype that SUCCEEDS cannot change what a later shape is
        // measured against.
        //
        // No `USING` clause. The renderer omits it for a generated column
        // (`cannot specify USING when altering type of generated column`), and for
        // every other shape here the implicit assignment cast is what a bare retype
        // does. Adding one would make `gen_out` fail for a reason that has nothing
        // to do with its companions.
        session.batch("BEGIN").await.expect("open probe txn");
        let attempted = session
            .batch(&format!(
                "ALTER TABLE {schema}.{table} ALTER COLUMN {column} TYPE {target}"
            ))
            .await;
        session.batch("ROLLBACK").await.expect("close probe txn");
        let refused = attempted.is_err();

        assert_eq!(
            refused,
            *expected_refusal,
            "PostgreSQL changed its mind about retyping a column with this companion \
             ({table}.{column}); the gate's rule is derived from this behaviour, so \
             re-measure before trusting it. Server said: {:?}",
            attempted.err().map(|e| e.to_string())
        );
        assert_eq!(
            predicted,
            refused,
            "column_type_change_blockers disagrees with the server for {table}.{column}: \
             it named {blockers:?}, actual refuse={refused}. The shipped gate would {} here",
            if predicted {
                "REJECT A MIGRATION POSTGRESQL ACCEPTS"
            } else {
                "wave through a retype PostgreSQL rejects, half-applying the plan"
            }
        );
        checked += 1;
        if refused {
            refused_shapes += 1;
        } else {
            accepted_shapes += 1;
        }
    }

    assert_eq!(
        checked,
        SHAPES.len(),
        "every shape was compared; a loop that skipped one would prove nothing about it"
    );
    // Both populations must be non-trivial. A predicate that refused everything, and
    // one that refused nothing, each agree with the server on one of these two sets
    // and are each catastrophically wrong; only having both makes the agreement above
    // mean the predicate discriminates.
    assert!(
        refused_shapes >= 5 && accepted_shapes >= 10,
        "the oracle needs both populations to be meaningful: {refused_shapes} refused, \
         {accepted_shapes} accepted"
    );

    session
        .batch(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .expect("drop the fixture");
}

/// The drop predicate and the retype predicate must NOT be interchangeable.
///
/// Every agreement in the oracle above is also an agreement the drop predicate
/// would report on most shapes, so "they agree with the server" is not by itself
/// evidence that a second predicate was needed. This asserts the difference
/// directly, on the two columns where it bites, and it is what would fail if
/// somebody later collapsed the two queries back into one.
#[compio::test]
async fn the_retype_predicate_is_not_the_drop_predicate() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("zm_retype_vs_drop_{}", std::process::id());
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    session
        .batch(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .await
        .expect("clear any leftover fixture");
    session
        .batch(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.t (fk_target int, k int,
               CONSTRAINT uq UNIQUE (fk_target));
             CREATE TABLE {schema}.child (r int,
               CONSTRAINT fk_r FOREIGN KEY (r) REFERENCES {schema}.t (fk_target));
             CREATE TABLE {schema}.part (k int, other int) PARTITION BY RANGE (k)"
        ))
        .await
        .expect("build the disagreement fixture");

    let cfg = ExecutorConfig::new(schema.clone(), &schema, support::no_inject(&schema));
    let backend = PostgresBackend::new_generic(&session);

    // Direction 1: the drop predicate would OVER-refuse a retype.
    let drop_says = backend
        .blocking_column_dependents(&cfg, "t", "fk_target")
        .await
        .expect("drop predicate runs");
    let retype_says = backend
        .column_type_change_blockers(&cfg, "t", "fk_target")
        .await
        .expect("retype predicate runs");
    assert!(
        !drop_says.is_empty(),
        "an inbound FOREIGN KEY must still block a DROP, or this comparison has lost \
         its subject: {drop_says:?}"
    );
    assert!(
        retype_says.is_empty(),
        "an inbound FOREIGN KEY does NOT block a retype - PostgreSQL accepts it, \
         measured. Refusing here would reject a migration the server honours: \
         {retype_says:?}"
    );

    // Direction 2: the drop predicate would UNDER-refuse a retype.
    let drop_says = backend
        .blocking_column_dependents(&cfg, "part", "k")
        .await
        .expect("drop predicate runs");
    let retype_says = backend
        .column_type_change_blockers(&cfg, "part", "k")
        .await
        .expect("retype predicate runs");
    assert!(
        drop_says.is_empty(),
        "a partition-key column leaves no blocking dependency edge, so the drop \
         predicate sees nothing - that is the point: {drop_says:?}"
    );
    assert!(
        !retype_says.is_empty(),
        "a partition-key column IS refused for a retype, and no pg_depend walk can \
         see it. A dependency-only predicate half-applies the plan here"
    );

    session
        .batch(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .expect("drop the fixture");
}
