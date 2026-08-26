//! A CHECK constraint rewritten out-of-band over DIFFERENT columns is drift.
//!
//! # The exemption this measures the edge of
//!
//! `apply::drift::constraint_definition_is_comparable` returns `false` for `CHECK` and
//! `EXCLUDE`, and it is right to: PostgreSQL deparses a CHECK body from the parse tree
//! rather than echoing what was written, so `CHECK (qty > 0)` reads back as
//! `CHECK ((qty > 0))` and `CHECK (code = 'x')` as `CHECK ((code = 'x'::text))`. An
//! offline renderer knows no column types and quotes unconditionally, so a byte compare
//! reports drift on every CHECK that exists. Exempting the text is the only honest
//! option, and the sibling files here pin the same concession for index predicates and
//! for vendor-object identities.
//!
//! # What the exemption is supposed to leave behind
//!
//! Its own family does not stop at exempting. `index_expression_bodies_are_comparable`
//! exempts an index body and hands the question to `index_referenced_columns` — WHICH
//! table columns the index reads — compared exactly where the body was exempted, so the
//! concession costs the logic and keeps the shape. `VendorObjectIdentities` goes further
//! for functions, policies and triggers: the deparsed text is never collected at all,
//! and what is compared is structural (a `polcmd` code, a `tgtype` bit set).
//!
//! The CHECK member has no such replacement. At the constraint compare, exempting the
//! definition leaves `kind` and `comment`, and `kind` is `CHECK` on both sides of any
//! rewrite. So a constraint that keeps its name can have its entire predicate replaced —
//! different operator, different literal, DIFFERENT COLUMNS — and the diff is clean.
//!
//! # Why the columns are the right thing to compare
//!
//! The same reason they are for an index. A CHECK's referenced attributes are stored
//! STRUCTURALLY in `pg_constraint.conkey`, as attribute numbers, so they survive the
//! deparse that eats the text and they follow a `RENAME COLUMN` on their own. And the
//! offline side can produce them without parsing anything: `IrConstraintKind::Check`
//! carries a closed `Expr` AST, not a SQL string, so walking it for column references is
//! ordinary neutral engine work — no vendor parser, no text.
//!
//! This does NOT recover the full loss. Two predicates over the SAME column with
//! different logic — `qty > 0` against `qty > -2147483648` — still compare equal, which
//! is exactly the bound the index member already states about itself. Recovering that
//! needs the catalog text parsed back to the closed AST. The claim here is narrower and
//! is the one the CHECK member is currently missing entirely: a rewrite that moves to
//! other columns changes the shape, and the shape is comparable.

use crate::support;

use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zeroship_migrate::{
    diff_snapshots, fold_ops, IrAuthor, LiveSchema, SchemaSnapshot, StructuralDrift,
};
use zeroship_migrate_postgres::backend::drift_sql::snapshot_schema;

const OWNER: &str = "app_drift_check_body";
const TABLE: &str = "check_body_probe";
const CONSTRAINT: &str = "check_body_probe_qty_positive";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drift_check_body_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// One table with two integer columns and a named CHECK over exactly one of them.
///
/// Two columns rather than one on purpose: the mutation below moves the predicate from
/// `qty` to `price`, and a single-column fixture could not express "different columns"
/// at all. `qty > 0` is deliberately the plainest possible predicate — nothing here is
/// testing expression richness, and a complicated body would make a failure ambiguous
/// between the exemption and the renderer.
fn fixture(schema: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "drift_check_body_pg",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": TABLE,
                "schema": schema,
                "columns": [
                    { "name": "qty", "type": "int", "nullable": false },
                    { "name": "price", "type": "int", "nullable": false }
                ],
                "constraints": [
                    {
                        "name": CONSTRAINT,
                        "kind": {
                            "kind": "check",
                            "expr": {
                                "node": "binOp",
                                "op": "gt",
                                "lhs": { "node": "colRef", "name": "qty" },
                                "rhs": { "node": "literal", "value": 0 }
                            }
                        }
                    }
                ]
            }
        ]
    }))
    .expect("PostgreSQL check-body fixture must deserialize")
}

async fn snapshot_after_mutation(
    session: &support::PgDevSession,
    schema: &str,
    mutation: &str,
) -> Result<SchemaSnapshot, String> {
    session
        .batch("BEGIN")
        .await
        .map_err(|error| format!("begin check-body mutation: {error}"))?;
    if let Err(error) = session.batch(mutation).await {
        let _ = session.batch("ROLLBACK").await;
        return Err(format!("apply check-body mutation `{mutation}`: {error}"));
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

fn drift_mentions_the_constraint(drift: &StructuralDrift) -> bool {
    let object = format!("constraint {CONSTRAINT}");
    drift
        .altered_objects
        .iter()
        .any(|altered| altered.object == object)
}

#[compio::test]
async fn live_postgres_reports_a_check_body_moved_to_another_column() {
    let url = require_live_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token();
    let quoted_schema = quote_ident(&schema);
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated check-body schema");

    let result: Result<(), String> = async {
        let ir = fixture(&schema);
        let expected = fold_ops(
            zeroship_migrate::shipping_vendors(),
            &ir.ops,
            &zeroship_migrate_postgres::DIALECT,
            &schema,
            &support::operator_charter("app"),
        )
        .map_err(|error| format!("fold check-body fixture: {error}"))?;
        let migrations = IrAuthor::new(
            zeroship_migrate::shipping_vendors(),
            &schema,
            OWNER,
            &zeroship_migrate_postgres::DIALECT,
            &support::operator_charter(&schema),
        )
        .lower(&ir, &LiveSchema::default())
        .map_err(|error| format!("lower check-body fixture: {error}"))?;
        for migration in &migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply {}: {error}", migration.name))?;
        }

        // THE FALSE-DRIFT CONTROL, and it is why the text is exempted in the first
        // place. PostgreSQL stores `CHECK ((qty > 0))` for what was authored as
        // `qty > 0`, so a comparison that read the text would report this untouched
        // table as drifted immediately. If this assertion ever fails, the exemption
        // has been removed rather than replaced and the fix below is the wrong one.
        let clean = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect clean check-body fixture: {error}"))?;
        let clean_drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &clean);
        if !clean_drift.is_clean() {
            return Err(format!(
                "clean check-body fixture drifted: {clean_drift:#?}"
            ));
        }

        // THE MUTATION. Same constraint name, same `CHECK` kind, a predicate over a
        // DIFFERENT COLUMN. Nothing an identity, type or index diff can see, and
        // nothing the exempted text is allowed to report.
        let mutation = format!(
            "ALTER TABLE {quoted_schema}.{table} DROP CONSTRAINT {constraint}; \
             ALTER TABLE {quoted_schema}.{table} ADD CONSTRAINT {constraint} \
             CHECK (price > 0)",
            table = quote_ident(TABLE),
            constraint = quote_ident(CONSTRAINT),
        );
        let mutated = snapshot_after_mutation(&session, &schema, &mutation).await?;
        let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &mutated);
        if !drift_mentions_the_constraint(&drift) {
            return Err(format!(
                "a CHECK moved from `qty` to `price` was NOT reported. The constraint \
                 keeps its name and its `CHECK` kind, so exempting the body leaves \
                 nothing that differs — which is the gap the index member of this same \
                 family closed with `index_referenced_columns`. Drift was: {drift:#?}"
            ));
        }

        Ok(())
    }
    .await;

    result.expect("live PostgreSQL check-body drift");
}
