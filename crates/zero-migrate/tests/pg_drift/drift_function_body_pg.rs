//! Live PostgreSQL regressions for the FUNCTION BODY drift surface.
//!
//! `vendor_object_drift` pins the IDENTITY half - a function's schema, name and
//! canonicalised argument vector - and identity is all the differ ever compared. So
//! `CREATE OR REPLACE FUNCTION` run out of band with the SAME signature and a
//! DIFFERENT body reported the schema CLEAN. Replacing the body without touching
//! the signature is the ordinary way a function is changed, so that was not an edge
//! case: it was the common one.
//!
//! WHY A BODY IS COMPARABLE WHERE A POLICY PREDICATE IS NOT. The vendor-object work
//! excluded function bodies, policy `USING`/`WITH CHECK` and trigger `WHEN`
//! together, on the grounds that PostgreSQL does not store SQL as written. That is
//! true of the predicates and FALSE of the body. Measured on PostgreSQL 18.4:
//!
//! ```text
//!   authored  AS $$ SELECT x+1 $$            prosrc  [ SELECT x+1 ]
//!   authored  AS $$BEGIN
//!                RETURN   42;
//!             END$$                          prosrc  [BEGIN\n   RETURN   42;\nEND]
//!   authored  USING (owner = current_user)    pg_get_expr  ((owner = CURRENT_USER))
//! ```
//!
//! `prosrc` is the authored text BYTE FOR BYTE - leading and trailing spaces, odd
//! internal whitespace, and a nested `$tag$` literal all survive - because
//! PostgreSQL stores a `LANGUAGE sql`/`plpgsql` body as an opaque string and hands
//! it to the language handler at call time. A policy predicate is a parse tree that
//! `pg_get_expr` re-prints. So the body needs no normaliser and the predicates
//! cannot have one. Policies and triggers are deliberately untouched here.
//!
//! THE ONE EXCEPTION, also measured: a SQL-standard-body function
//! (`BEGIN ATOMIC ... END`) keeps its body as a PARSE TREE in `prosqlbody` and
//! leaves `prosrc` EMPTY. Comparing an authored body against `""` would report
//! every such function as drifted, so that shape must DECLINE - which this file
//! pins by converting an authored function into one out of band.
//!
//! The suite is gated by `ZERO_MIGRATE_TEST_PG_URL`. Every mutation runs in a
//! transaction that is rolled back after introspection, so all assertions share one
//! authoritative folded snapshot without mutation order coupling.

use crate::support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    diff_snapshots, fold_ops, snapshot_schema, IrAuthor, LiveSchema, SchemaSnapshot, SqlDialect,
    StructuralDrift,
};

const OWNER: &str = "app_drift_function_body";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drift_function_body_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Four function bodies, each chosen for a different half of the claim.
///
///   * `plain_sql` is the ordinary `LANGUAGE sql` body - the shape an out-of-band
///     `CREATE OR REPLACE` rewrites most often.
///   * `plpgsql_body` carries odd INTERNAL whitespace, which `prosrc` preserves and
///     no normaliser may be allowed to touch.
///   * `dollar_quoted` embeds a nested `$tag$` literal containing a literal `$$`.
///     It is the false-drift control for the renderer's own dollar tag: the body is
///     wrapped in `$zsfn$ … $zsfn$` at lower, and a body that could terminate that
///     quote early - or that a normaliser tried to unwrap - would corrupt here
///     first.
///   * `replaced_body` is authored TWICE, the second time with `replace: true`, so
///     the expected side has to hold the LAST body rather than the first. A fold
///     that kept the first would report drift against a schema that matches the
///     history exactly.
///
/// No `BEGIN ATOMIC` function is authored, because the IR cannot express one: the
/// renderer always emits `AS $zsfn$ … $zsfn$`. That shape is reached the only way a
/// real project reaches it - a hand-run `CREATE OR REPLACE` - below.
fn fixture(schema: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "drift_function_body_pg",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createFunction",
                "name": "plain_sql",
                "schema": schema,
                "args": [{ "name": "x", "type": "int" }],
                "returns": "int",
                "language": "sql",
                "body": "SELECT x + 1"
            },
            {
                "op": "createFunction",
                "name": "plpgsql_body",
                "schema": schema,
                "returns": "int",
                "language": "plpgsql",
                "body": "BEGIN\n   RETURN   42;\nEND"
            },
            {
                "op": "createFunction",
                "name": "dollar_quoted",
                "schema": schema,
                "returns": "text",
                "language": "plpgsql",
                "body": "BEGIN\n  RETURN $tag$he said $$hi$$ to me$tag$;\nEND"
            },
            {
                "op": "createFunction",
                "name": "replaced_body",
                "schema": schema,
                "returns": "int",
                "language": "sql",
                "body": "SELECT 1"
            },
            {
                "op": "createFunction",
                "name": "replaced_body",
                "schema": schema,
                "returns": "int",
                "language": "sql",
                "replace": true,
                "body": "SELECT 2"
            }
        ]
    }))
    .expect("PostgreSQL function-body fixture must deserialize")
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

/// `arg_types` is the CANONICAL spelling `FunctionKey::canonicalized` lands on, not
/// the authored one and not `format_type`'s: an authored `int` and a catalog
/// `integer` both reduce to `int4`, which is what the drift label prints.
fn require_body_drift(
    drift: &StructuralDrift,
    schema: &str,
    function: &str,
    arg_types: &str,
    expected_contains: &str,
    actual_contains: &str,
) -> Result<(), String> {
    let object = format!("function {schema}.{function}({arg_types})");
    if drift.altered_objects.iter().any(|altered| {
        altered.object == object
            && altered.field == "body"
            && altered.expected.contains(expected_contains)
            && altered.actual.contains(actual_contains)
    }) {
        Ok(())
    } else {
        Err(format!(
            "missing `{object}` body drift (expected containing {expected_contains:?}, \
             actual containing {actual_contains:?}): {drift:#?}"
        ))
    }
}

fn require_no_body_drift(
    drift: &StructuralDrift,
    schema: &str,
    function: &str,
    arg_types: &str,
    why: &str,
) -> Result<(), String> {
    let object = format!("function {schema}.{function}({arg_types})");
    if drift
        .altered_objects
        .iter()
        .any(|altered| altered.object == object && altered.field == "body")
    {
        Err(format!(
            "`{object}` reported body drift it cannot soundly claim - {why}: {drift:#?}"
        ))
    } else {
        Ok(())
    }
}

/// A body comparison must never disturb the IDENTITY half. If a mutation made the
/// two sides key differently, the body comparison would silently stop running and
/// every `require_no_body_drift` above would pass VACUOUSLY.
fn require_function_still_paired(
    drift: &StructuralDrift,
    schema: &str,
    function: &str,
    arg_types: &str,
) -> Result<(), String> {
    let object = format!("function {schema}.{function}({arg_types})");
    if drift.missing_objects.iter().any(|m| m == &object)
        || drift.unexpected_objects.iter().any(|u| u == &object)
    {
        return Err(format!(
            "`{object}` fell out of the identity pairing, so any body assertion about \
             it is vacuous: {drift:#?}"
        ));
    }
    Ok(())
}

#[compio::test]
async fn live_postgres_reports_function_body_drift() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token();
    let quoted_schema = quote_ident(&schema);
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated function-body schema");

    let result: Result<(), String> = async {
        let ir = fixture(&schema);
        // `operator_charter` rather than `no_inject`: a function is a charter-gated
        // vendor primitive, and under the default charter this fixture is refused at
        // policy load with VENDOR_OP_DENIED - above the fold, before anything this
        // file measures runs.
        let expected = fold_ops(
            &ir.ops,
            SqlDialect::Postgres,
            &schema,
            &support::operator_charter("app"),
        )
        .map_err(|error| format!("fold function-body fixture: {error}"))?;
        let migrations = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::operator_charter(&schema),
        )
        .lower(&ir, &LiveSchema::default())
        .map_err(|error| format!("lower function-body fixture: {error}"))?;
        for migration in &migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply {}: {error}", migration.name))?;
        }

        // THE FALSE-DRIFT CONTROL, and it is the assertion that matters most. The
        // renderer wraps the authored body in `$zsfn$\n … \n$zsfn$`, so what
        // PostgreSQL stores is the authored text with a newline added at each end.
        // A comparison that did not account for that would report all four of these
        // functions as drifted, immediately, on a schema nobody has touched - which
        // is strictly worse than the blind spot it replaces.
        let clean = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect clean function-body fixture: {error}"))?;
        let clean_drift = diff_snapshots(&expected, &clean);
        if !clean_drift.is_clean() {
            return Err(format!(
                "clean function-body fixture drifted: {clean_drift:#?}"
            ));
        }

        // A `CREATE OR REPLACE` with the SAME signature and a DIFFERENT body. This
        // changes what every caller of the function computes and changes NOTHING an
        // identity, type, constraint or index diff can see.
        for (function, arg_types, mutation, expected_key, actual_key) in [
            (
                "plain_sql",
                "int4",
                format!(
                    "CREATE OR REPLACE FUNCTION {quoted_schema}.plain_sql(x int) \
                     RETURNS int LANGUAGE sql AS $$ SELECT x + 999 $$"
                ),
                "x + 1",
                "x + 999",
            ),
            (
                "plpgsql_body",
                "",
                format!(
                    "CREATE OR REPLACE FUNCTION {quoted_schema}.plpgsql_body() \
                     RETURNS int LANGUAGE plpgsql AS $$BEGIN RETURN 7; END$$"
                ),
                "RETURN   42;",
                "RETURN 7;",
            ),
            (
                "dollar_quoted",
                "",
                format!(
                    "CREATE OR REPLACE FUNCTION {quoted_schema}.dollar_quoted() \
                     RETURNS text LANGUAGE plpgsql AS $outer$BEGIN \
                     RETURN $tag$she said $$bye$$ instead$tag$; END$outer$"
                ),
                "he said",
                "she said",
            ),
            (
                "replaced_body",
                "",
                format!(
                    "CREATE OR REPLACE FUNCTION {quoted_schema}.replaced_body() \
                     RETURNS int LANGUAGE sql AS $$ SELECT 3 $$"
                ),
                // The LAST authored body, not the first. `SELECT 1` here would mean
                // the fold kept the pre-replace definition.
                "SELECT 2",
                "SELECT 3",
            ),
        ] {
            let actual = snapshot_after_mutation(&session, &schema, &mutation).await?;
            let drift = diff_snapshots(&expected, &actual);
            require_function_still_paired(&drift, &schema, function, arg_types)?;
            require_body_drift(
                &drift,
                &schema,
                function,
                arg_types,
                expected_key,
                actual_key,
            )?;
        }

        // A body change that alters ONLY leading and trailing whitespace is
        // deliberately NOT reported. The renderer's own `$zsfn$\n … \n$zsfn$`
        // padding means the two sides never agree on the outer whitespace to begin
        // with, so the comparison trims both; a hand-run replace that adds more of
        // it therefore lands on the same key. Pinned as the documented give-up.
        let actual = snapshot_after_mutation(
            &session,
            &schema,
            &format!(
                "CREATE OR REPLACE FUNCTION {quoted_schema}.plain_sql(x int) \
                 RETURNS int LANGUAGE sql AS $$\n\n   SELECT x + 1   \n\n$$"
            ),
        )
        .await?;
        let drift = diff_snapshots(&expected, &actual);
        require_function_still_paired(&drift, &schema, "plain_sql", "integer")?;
        require_no_body_drift(
            &drift,
            &schema,
            "plain_sql",
            "int4",
            "only the leading and trailing whitespace changed, which the renderer's own \
             dollar-tag padding already makes incomparable",
        )?;

        // THE DECLINE, reached the only way a real project reaches it. A
        // SQL-standard-body function keeps its body as a parse tree in `prosqlbody`
        // and leaves `prosrc` EMPTY. Measured on PostgreSQL 18.4:
        //
        //     CREATE FUNCTION f(x int) RETURNS int LANGUAGE sql
        //     BEGIN ATOMIC SELECT x+1; END;
        //         prosrc = []   prosqlbody IS NOT NULL
        //
        // So the actual side has NO comparable body, and comparing the authored one
        // against `""` would report a function that may well be equivalent. The
        // identity pairing must survive - this is a decline, not a key mismatch -
        // and the body must not be reported.
        let actual = snapshot_after_mutation(
            &session,
            &schema,
            &format!(
                "CREATE OR REPLACE FUNCTION {quoted_schema}.plain_sql(x int) \
                 RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT x + 12345; END"
            ),
        )
        .await?;
        let drift = diff_snapshots(&expected, &actual);
        require_function_still_paired(&drift, &schema, "plain_sql", "integer")?;
        require_no_body_drift(
            &drift,
            &schema,
            "plain_sql",
            "int4",
            "a BEGIN ATOMIC body lives in `prosqlbody` as a parse tree and leaves `prosrc` \
             empty, so there is no authored text on the actual side to compare",
        )?;

        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!("DROP SCHEMA IF EXISTS {quoted_schema} CASCADE"))
        .await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => {
            panic!("live PostgreSQL function-body drift regression failed: {work}")
        }
        (Ok(()), Err(cleanup)) => panic!("drop function-body schema: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!(
            "live PostgreSQL function-body drift regression failed: {work}; \
             cleanup failed: {cleanup}"
        ),
    }
}
