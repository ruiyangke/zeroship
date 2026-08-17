//! Live PostgreSQL regressions for the ORDINARY column `DEFAULT` drift surface -
//! the defaults that carry no ID semantics at all.
//!
//! `drift_id_facets_pg` already pins the ID-bearing defaults (identity, UUID
//! generators, `nextval`, TypeID/ULID keys), which reach the differ through
//! `ColumnSnapshot::id_default`. This file pins the other half: a plain
//! `DEFAULT 42` / `DEFAULT 'active'` on a column with no ID facet, where the only
//! evidence either side carries is the raw SQL text.
//!
//! Both halves matter for the same reason. A default is what a row gets when the
//! writer says nothing, so an out-of-band `SET DEFAULT` silently changes what the
//! application stores without changing any type, constraint or index.
//!
//! The suite is gated by `ZERO_MIGRATE_TEST_PG_URL`. Every mutation runs in a
//! transaction that is rolled back after introspection, so all assertions share
//! one authoritative folded snapshot without mutation order coupling.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    diff_snapshots, fold_ops, snapshot_schema, IrAuthor, LiveSchema, SchemaSnapshot, SqlDialect,
    StructuralDrift,
};

const OWNER: &str = "app_drift_plain_default";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drift_plain_default_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// One table whose every column is an ordinary, non-ID default surface, plus the
/// two shapes PostgreSQL rewrites hardest.
///
/// `label`, `amount` and `enabled` exist to prove the NORMALISATION half:
/// PostgreSQL deparses `'active'` as `'active'::text` and reports `1.50` back with
/// its authored scale, so a byte compare of authored text against catalog text
/// would report drift on a schema nobody touched. `stamp` and `payload` are the
/// expression and container shapes the comparison declines to compare at all, and
/// `ticket`, `serial_id` and `derived` are the ID-bearing and generated surfaces
/// this comparison must leave to the paths that already own them.
fn fixture(schema: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "drift_plain_column_default_pg",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createSequence",
                "name": "plain_counter",
                "schema": schema,
                "as": "bigInt"
            },
            {
                "op": "createTable",
                "name": "plain_defaults",
                "columns": [
                    { "name": "quantity", "type": "int", "nullable": false,
                      "default": { "literal": { "value": 42 } } },
                    { "name": "label", "type": "text", "nullable": true,
                      "default": { "literal": { "value": "active" } } },
                    { "name": "enabled", "type": "boolean", "nullable": false,
                      "default": { "literal": { "value": true } } },
                    { "name": "amount", "type": { "decimal": { "precision": 12, "scale": 2 } },
                      "nullable": true,
                      "default": { "literal": { "value": "1.50" } } },
                    { "name": "undefaulted", "type": "int", "nullable": true },
                    { "name": "stamp", "type": "timestamp", "nullable": true,
                      "default": { "expr": { "node": "fnSynth", "fn": "now", "args": [] } } },
                    { "name": "payload", "type": "json", "nullable": true,
                      "default": { "container": "object" } },
                    { "name": "ticket", "type": "bigInt", "nullable": false,
                      "default": { "nextval": { "name": "plain_counter", "schema": schema } } },
                    { "name": "serial_id", "type": "bigInt", "nullable": false,
                      "identity": { "always": false } },
                    { "name": "source", "type": "int", "nullable": false },
                    { "name": "derived", "type": "int", "nullable": true,
                      "generated": {
                          "expr": {
                              "node": "binOp",
                              "op": "add",
                              "lhs": { "node": "colRef", "name": "source" },
                              "rhs": { "node": "literal", "value": 1 }
                          },
                          "stored": true
                      } }
                ],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            }
        ]
    }))
    .expect("PostgreSQL plain-default fixture must deserialize")
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

fn require_altered(
    drift: &StructuralDrift,
    column: &str,
    expected_contains: &str,
    actual_contains: &str,
) -> Result<(), String> {
    let object = format!("column {column}");
    if drift.altered_objects.iter().any(|altered| {
        altered.table == "plain_defaults"
            && altered.object == object
            && altered.field == "default"
            && altered.expected.contains(expected_contains)
            && altered.actual.contains(actual_contains)
    }) {
        Ok(())
    } else {
        Err(format!(
            "missing plain_defaults.{object} default drift \
             (expected containing {expected_contains:?}, actual containing {actual_contains:?}): \
             {drift:#?}"
        ))
    }
}

fn require_no_default_drift(drift: &StructuralDrift, column: &str) -> Result<(), String> {
    let object = format!("column {column}");
    if drift
        .altered_objects
        .iter()
        .any(|altered| altered.object == object && altered.field == "default")
    {
        Err(format!(
            "plain_defaults.{object} reported default drift it cannot soundly claim: {drift:#?}"
        ))
    } else {
        Ok(())
    }
}

#[compio::test]
async fn live_postgres_reports_ordinary_column_default_drift() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token();
    let quoted_schema = quote_ident(&schema);
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated plain-default schema");

    let result: Result<(), String> = async {
        let ir = fixture(&schema);
        let expected = fold_ops(
            &ir.ops,
            SqlDialect::Postgres,
            &schema,
            &support::no_inject("app"),
        )
        .map_err(|error| format!("fold plain-default fixture: {error}"))?;
        let migrations = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::no_inject(&schema),
        )
        .lower(&ir, &LiveSchema::default())
        .map_err(|error| format!("lower plain-default fixture: {error}"))?;
        for migration in &migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply {}: {error}", migration.name))?;
        }

        // PostgreSQL rewrites what it stores: `'active'` reads back as
        // `'active'::text`, `{}` as `'{}'::jsonb`. A default comparison that
        // compared authored text against catalog text would report every one of
        // these columns as drifted on a schema nobody has touched.
        let clean = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect clean plain-default fixture: {error}"))?;
        let clean_drift = diff_snapshots(&expected, &clean);
        if !clean_drift.is_clean() {
            return Err(format!(
                "clean plain-default fixture drifted: {clean_drift:#?}"
            ));
        }

        let table = format!("{quoted_schema}.{}", quote_ident("plain_defaults"));
        // Each of these is an out-of-band `ALTER COLUMN` that changes what a row
        // gets when the writer says nothing, and changes NOTHING a type,
        // constraint or index diff can see.
        for (column, mutation, expected_key, actual_key) in [
            (
                "quantity",
                format!("ALTER TABLE {table} ALTER COLUMN quantity SET DEFAULT 7"),
                "42",
                "7",
            ),
            (
                "label",
                format!("ALTER TABLE {table} ALTER COLUMN label SET DEFAULT 'inactive'"),
                "active",
                "inactive",
            ),
            (
                "enabled",
                format!("ALTER TABLE {table} ALTER COLUMN enabled SET DEFAULT false"),
                "true",
                "false",
            ),
            (
                "amount",
                format!("ALTER TABLE {table} ALTER COLUMN amount SET DEFAULT 2.50"),
                "1.50",
                "2.50",
            ),
            (
                "undefaulted",
                format!("ALTER TABLE {table} ALTER COLUMN undefaulted SET DEFAULT 99"),
                "absent",
                "99",
            ),
            (
                "label",
                format!("ALTER TABLE {table} ALTER COLUMN label DROP DEFAULT"),
                "active",
                "absent",
            ),
        ] {
            let actual = snapshot_after_mutation(&session, &schema, &mutation).await?;
            require_altered(
                &diff_snapshots(&expected, &actual),
                column,
                expected_key,
                actual_key,
            )?;
        }

        // A default REPLACED by an expression, and an expression default
        // replaced by a literal, are still presence-stable - both sides have a
        // default - so they land on the declined side of
        // `comparable_column_default`. Pinned as the boundary the predicate
        // documents, not as an aspiration.
        for (column, mutation) in [
            (
                "stamp",
                format!("ALTER TABLE {table} ALTER COLUMN stamp SET DEFAULT clock_timestamp()"),
            ),
            (
                "payload",
                format!("ALTER TABLE {table} ALTER COLUMN payload SET DEFAULT '[]'::jsonb"),
            ),
        ] {
            let actual = snapshot_after_mutation(&session, &schema, &mutation).await?;
            require_no_default_drift(&diff_snapshots(&expected, &actual), column)?;
        }

        // A GENERATED column is a SEPARATE and still-open gap, recorded here so
        // nobody has to re-measure it. `ColumnSnapshot::generated` is populated by
        // the fold and left `None` by live PostgreSQL introspection, which reads
        // `attgenerated` only to keep a generated expression out of the default
        // surface. So neither the expression nor the generated-ness itself is
        // compared, and both of these are silent. Closing it needs two things this
        // change does not do: introspection that COLLECTS the expression, and a
        // sound way to compare it - `pg_get_expr` returns `(source + 1)` for the
        // `(("source" + 1))` the offline renderer emits, the same deparse problem
        // `constraint_definition_is_comparable` already refuses at.
        for mutation in [
            format!("ALTER TABLE {table} ALTER COLUMN derived SET EXPRESSION AS (source + 2)"),
            format!("ALTER TABLE {table} ALTER COLUMN derived DROP EXPRESSION"),
        ] {
            let actual = snapshot_after_mutation(&session, &schema, &mutation).await?;
            let drift = diff_snapshots(&expected, &actual);
            if !drift.is_clean() {
                return Err(format!(
                    "`{mutation}` now reports drift - the generated-column gap this pins has \
                     been closed, so replace this block with the assertion it earned: {drift:#?}"
                ));
            }
        }

        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!("DROP SCHEMA IF EXISTS {quoted_schema} CASCADE"))
        .await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("live PostgreSQL plain-default drift regression failed: {work}"),
        (Ok(()), Err(cleanup)) => panic!("drop plain-default schema: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!(
            "live PostgreSQL plain-default drift regression failed: {work}; cleanup failed: {cleanup}"
        ),
    }
}
