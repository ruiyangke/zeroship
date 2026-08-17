//! Live PostgreSQL regression for the NO-OP partial-index predicate.
//!
//! PostgreSQL does not store a partial index's `WHERE` clause as written. It holds
//! `pg_index.indpred` as a parse tree, and for the bare constant `TRUE` it stores
//! NOTHING AT ALL - the index is recorded as total. Measured on PostgreSQL 18.4,
//! immediately after `CREATE INDEX`:
//!
//! ```text
//! WHERE TRUE            -> indpred IS NULL      (the predicate is gone)
//! WHERE FALSE           -> false
//! WHERE (1 = 1)         -> (1 = 1)
//! WHERE (TRUE AND TRUE) -> (true AND true)
//! WHERE (note IS NOT NULL) -> (note IS NOT NULL)
//! ```
//!
//! Only the bare constant is dropped; every other predicate, including the ones that
//! are semantically constant, survives verbatim. That is a narrow, exact rule.
//!
//! The differ compares predicate PRESENCE (`Some` against `None`) whenever it declines
//! to compare the two bodies, which on PostgreSQL is always
//! (`apply::drift::index_expression_bodies_are_comparable`). So a `WHERE true` index
//! reported `predicate: expected "TRUE", actual ""` on the FIRST introspection after a
//! clean apply, forever, with nothing in the history but the `createIndex` that built
//! it. That is the false drift this file pins closed.
//!
//! The fix reduces BOTH sides through one key rather than declining: a predicate whose
//! whole text is the constant TRUE is not a predicate, because the server says so. The
//! last test is what keeps that from being a licence to ignore predicates - a genuine
//! presence difference must still report.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    diff_snapshots, fold_ops, snapshot_schema, IrAuthor, LiveSchema, SchemaSnapshot, SqlDialect,
    StructuralDrift,
};

const OWNER: &str = "app_drift_noop_predicate";

const TABLE: &str = "noop_predicate";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drift_noop_pred_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Three partial indexes: the bare constant PostgreSQL discards, a semantically
/// constant predicate it KEEPS (so the reduction cannot be "drop anything that looks
/// tautological"), and an ordinary one that reads a column.
fn fixture() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "drift_noop_index_predicate_pg",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": TABLE,
                "columns": [
                    { "name": "id", "type": "int", "nullable": false },
                    { "name": "note", "type": "text", "nullable": true },
                    { "name": "qty", "type": "int", "nullable": true }
                ],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createIndex", "table": TABLE, "name": "noop_predicate_true",
                "columns": [{"kind":"column","name":"note"}], "unique": false,
                "where": {"node":"literal","value":true}
            },
            {
                "op": "createIndex", "table": TABLE, "name": "noop_predicate_kept",
                "columns": [{"kind":"column","name":"qty"}], "unique": false,
                "where": {"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"qty"},
                          "rhs":{"node":"literal","value":0}}
            }
        ]
    }))
    .expect("no-op predicate fixture must deserialize")
}

async fn snapshot_after_mutation(
    session: &support::PgDevSession,
    schema: &str,
    mutation: &str,
) -> Result<SchemaSnapshot, String> {
    session
        .batch("BEGIN")
        .await
        .map_err(|error| format!("begin drift mutation: {error}"))?;
    if let Err(error) = session.batch(mutation).await {
        let _ = session.batch("ROLLBACK").await;
        return Err(format!("apply drift mutation `{mutation}`: {error}"));
    }
    let snapshot = snapshot_schema(session, schema)
        .await
        .map_err(|error| format!("snapshot after `{mutation}`: {error}"));
    let rollback = session
        .batch("ROLLBACK")
        .await
        .map_err(|error| format!("rollback `{mutation}`: {error}"));
    match (snapshot, rollback) {
        (Ok(snapshot), Ok(())) => Ok(snapshot),
        (Err(snapshot), Ok(())) => Err(snapshot),
        (Ok(_), Err(rollback)) => Err(rollback),
        (Err(snapshot), Err(rollback)) => Err(format!("{snapshot}; {rollback}")),
    }
}

fn require_predicate_drift(drift: &StructuralDrift, index: &str) -> Result<(), String> {
    let object = format!("index {index}");
    if drift
        .altered_objects
        .iter()
        .any(|altered| altered.object == object && altered.field == "predicate")
    {
        Ok(())
    } else {
        Err(format!(
            "{TABLE}.{object} lost its predicate live and reported no `predicate` drift: \
             {drift:#?}"
        ))
    }
}

#[compio::test]
async fn live_postgres_does_not_invent_drift_for_a_constant_true_predicate() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token();
    let quoted_schema = quote_ident(&schema);
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated no-op predicate schema");

    let result: Result<(), String> = async {
        let ir = fixture();
        let expected = fold_ops(
            &ir.ops,
            SqlDialect::Postgres,
            &schema,
            &support::no_inject("app"),
        )
        .map_err(|error| format!("fold no-op predicate fixture: {error}"))?;
        let migrations = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::no_inject(&schema),
        )
        .lower(&ir, &LiveSchema::default())
        .map_err(|error| format!("lower no-op predicate fixture: {error}"))?;
        for migration in &migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply {}: {error}", migration.name))?;
        }

        // The witness that gives this test a subject: the fold really does carry a
        // predicate PostgreSQL really does not.
        let folded_predicate = expected
            .tables
            .get(TABLE)
            .and_then(|table| {
                table
                    .indexes
                    .iter()
                    .find(|index| index.name == "noop_predicate_true")
            })
            .and_then(|index| index.predicate.clone());
        if folded_predicate.is_none() {
            return Err(
                "the fold must project the authored `WHERE true` predicate for this test to \
                 have a subject; it projected none"
                    .to_string(),
            );
        }

        // The false-drift control: apply, change NOTHING, demand silence.
        let clean = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect clean no-op predicate fixture: {error}"))?;
        let live_predicate = clean
            .tables
            .get(TABLE)
            .and_then(|table| {
                table
                    .indexes
                    .iter()
                    .find(|index| index.name == "noop_predicate_true")
            })
            .and_then(|index| index.predicate.clone());
        if live_predicate.is_some() {
            return Err(format!(
                "PostgreSQL must record NO predicate for `WHERE true`, otherwise the two sides \
                 never disagreed and this test proves nothing; it recorded {live_predicate:?}"
            ));
        }
        let clean_drift = diff_snapshots(&expected, &clean);
        if !clean_drift.is_clean() {
            return Err(format!(
                "a `WHERE true` partial index drifted against the database that had just been \
                 built from it: {clean_drift:#?}"
            ));
        }

        // What keeps the reduction from being a licence to stop comparing predicates:
        // an index that genuinely LOSES its predicate out of band must still report.
        let mutation = format!(
            "DROP INDEX {quoted_schema}.{}; CREATE INDEX {} ON {quoted_schema}.{} (\"qty\")",
            quote_ident("noop_predicate_kept"),
            quote_ident("noop_predicate_kept"),
            quote_ident(TABLE),
        );
        let actual = snapshot_after_mutation(&session, &schema, &mutation).await?;
        require_predicate_drift(&diff_snapshots(&expected, &actual), "noop_predicate_kept")?;

        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!("DROP SCHEMA IF EXISTS {quoted_schema} CASCADE"))
        .await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("live PostgreSQL no-op predicate regression failed: {work}"),
        (Ok(()), Err(cleanup)) => panic!("drop no-op predicate schema: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!(
            "live PostgreSQL no-op predicate regression failed: {work}; cleanup failed: {cleanup}"
        ),
    }
}
