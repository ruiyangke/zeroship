//! Live PostgreSQL regressions for the VIEW BODY drift surface.
//!
//! `diff_snapshots` compared a view on exactly two fields - `materialized` and
//! `comment` - and `ViewSnapshot`'s own `PartialEq` compared the same two. So a
//! `CREATE OR REPLACE VIEW` run out of band with the SAME name and a DIFFERENT body
//! reported the schema CLEAN. That is the ordinary way a view is changed, and it is
//! the exact shape this repo's effect model was built around: `CREATE OR REPLACE
//! VIEW` reads as a creation while silently recomputing the view's dependency
//! edges.
//!
//! WHY THIS NEEDED A DIFFERENT INSTRUMENT FROM THE FUNCTION BODY FIX. A function
//! body is comparable directly, because `pg_proc.prosrc` is the authored text byte
//! for byte - PostgreSQL keeps a `LANGUAGE sql`/`plpgsql` body as an opaque string.
//! A view body is NOT. PostgreSQL parses it, throws the text away, and `pg_get_viewdef`
//! RE-PRINTS it from the parse tree. Measured on PostgreSQL 18.4, authoring
//!
//! ```text
//!   SELECT "id", "amount" FROM "t" AS "tt" WHERE "amount" > 10
//! ```
//!
//! and reading it back yields
//!
//! ```text
//!    SELECT id,
//!      amount
//!     FROM t tt
//!    WHERE amount > 10::numeric;
//! ```
//!
//! - quoting dropped, `AS` in the alias dropped, whitespace reflowed onto four
//! lines with a leading space, a numeric cast inserted that nobody wrote, and a
//! trailing semicolon added. A hand-written normaliser that collapsed all of that
//! would have to erase casts and whitespace and quoting, and a normaliser aggressive
//! enough to do it is aggressive enough to erase the body change it exists to find.
//!
//! THE TWO SIDES ALSO DO NOT CARRY THE SAME FIELD. Measured, not assumed, by
//! `both_sides_of_a_view_body_are_measured` below: the introspected side fills
//! `definition` and leaves `authored_query` `None`; the folded side fills
//! `authored_query` and leaves `definition` `None`. There is NO field populated on
//! both sides, so no naive compare of either one was ever available - comparing
//! `definition` alone would report `None` against `Some(..)` and manufacture drift
//! on every view in existence.
//!
//! SO THE SERVER IS THE NORMALISER, AND THERE IS NO OTHER ONE. `resolve_view_bodies`
//! renders the authored body, hands it to PostgreSQL as a temporary view inside a
//! savepoint it always rolls back, and reads BOTH bodies back through
//! `pg_get_viewdef` in ONE statement. Both sides then hold the identical
//! deterministic re-print of the same server, and `diff_snapshots` compares them
//! with `==`. What it writes lands in `comparable_body`, never in `definition`:
//! `definition` on the folded side may be an introspected value cloned forward by a
//! catalog-seeded fold, and PostgreSQL follows a table rename into a dependent
//! view's stored body with no statement naming the view, so that clone goes stale
//! and comparing it reports drift on an untouched view.
//!
//! The suite is gated by `ZERO_MIGRATE_TEST_PG_URL`. Every out-of-band mutation runs
//! in a transaction that is rolled back after introspection, so all assertions share
//! one authoritative folded snapshot without mutation order coupling.

use crate::support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_view_bodies, snapshot_schema, IrAuthor, LiveSchema,
    SchemaSnapshot, SqlDialect, StructuralDrift,
};

const OWNER: &str = "app_drift_view_body";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drift_view_body_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// One backing table and four views, each chosen for a different half of the claim.
///
///   * `plain_projection` is the ordinary shape - a projection and nothing else.
///     It is the false-drift control for the server's re-print: the authored body
///     and the catalog body differ in whitespace and quoting on a schema nobody has
///     touched.
///   * `filtered` carries a WHERE against a `numeric` column, so the catalog body
///     gains a `::numeric` cast the author never wrote. It is the false-drift
///     control for inserted casts specifically.
///   * `aliased` names its FROM relation, which the server re-prints without the
///     `AS` keyword, and renames a projected column, which the server keeps.
///   * `replaced` is authored TWICE, the second time with `replace: true`, so the
///     expected side has to hold the LAST body. A fold that kept the first would
///     report drift against a schema matching its own history exactly.
fn fixture(schema: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "drift_view_body_pg",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "src",
                "schema": schema,
                "columns": [
                    { "name": "id", "type": "int", "nullable": false },
                    {
                        "name": "amount",
                        "type": { "decimal": { "precision": 12, "scale": 2 } },
                        "nullable": true
                    },
                    { "name": "label", "type": "text", "nullable": true }
                ],
                "primaryKey": ["id"]
            },
            {
                "op": "createView",
                "name": "plain_projection",
                "schema": schema,
                "query": { "kind": "structured", "select": {
                    "from": { "name": "src" },
                    "projection": [
                        { "kind": "colRef", "name": "id" },
                        { "kind": "colRef", "name": "label" }
                    ]
                }}
            },
            {
                "op": "createView",
                "name": "filtered",
                "schema": schema,
                "query": { "kind": "structured", "select": {
                    "from": { "name": "src" },
                    "projection": [{ "kind": "colRef", "name": "id" }],
                    "where": {
                        "node": "binOp",
                        "op": "gt",
                        "lhs": { "node": "colRef", "name": "amount" },
                        "rhs": { "node": "literal", "value": 10 }
                    }
                }}
            },
            {
                "op": "createView",
                "name": "aliased",
                "schema": schema,
                "query": { "kind": "structured", "select": {
                    "from": { "name": "src", "alias": "s" },
                    "projection": [
                        { "kind": "colRef", "table": "s", "name": "id", "alias": "the_id" },
                        { "kind": "colRef", "table": "s", "name": "label" }
                    ]
                }}
            },
            {
                "op": "createView",
                "name": "replaced",
                "schema": schema,
                "query": { "kind": "structured", "select": {
                    "from": { "name": "src" },
                    "projection": [{ "kind": "colRef", "name": "id" }]
                }}
            },
            {
                "op": "createView",
                "name": "replaced",
                "schema": schema,
                "replace": true,
                "query": { "kind": "structured", "select": {
                    "from": { "name": "src" },
                    "projection": [
                        { "kind": "colRef", "name": "id" },
                        { "kind": "colRef", "name": "label" }
                    ]
                }}
            }
        ]
    }))
    .expect("PostgreSQL view-body fixture must deserialize")
}

/// Introspect under an out-of-band mutation and hand back BOTH sides with their
/// bodies resolved.
///
/// The mutation and the resolve run inside the same transaction on purpose: the
/// probe has to re-print the authored body against the schema as the mutation left
/// it, and it has to prove it works when the caller already owns a transaction
/// block - the nested-savepoint half of `resolve_view_bodies`. The expected side is
/// cloned per mutation because the resolve writes into it.
async fn snapshot_after_mutation(
    session: &support::PgDevSession,
    schema: &str,
    expected: &SchemaSnapshot,
    mutation: &str,
) -> Result<(SchemaSnapshot, SchemaSnapshot), String> {
    session
        .batch("BEGIN")
        .await
        .map_err(|error| format!("begin drift mutation: {error}"))?;
    if let Err(error) = session.batch(mutation).await {
        let _ = session.batch("ROLLBACK").await;
        return Err(format!("apply drift mutation `{mutation}`: {error}"));
    }
    let resolved = async {
        let mut actual = snapshot_schema(session, schema)
            .await
            .map_err(|error| format!("snapshot after `{mutation}`: {error}"))?;
        let mut expected = expected.clone();
        resolve_view_bodies(session, schema, &mut expected, &mut actual)
            .await
            .map_err(|error| format!("resolve view bodies after `{mutation}`: {error}"))?;
        Ok::<_, String>((expected, actual))
    }
    .await;
    let rollback = session
        .batch("ROLLBACK")
        .await
        .map_err(|error| format!("rollback `{mutation}`: {error}"));
    match (resolved, rollback) {
        (Ok(pair), Ok(())) => Ok(pair),
        (Err(resolved), Ok(())) => Err(resolved),
        (Ok(_), Err(rollback)) => Err(rollback),
        (Err(resolved), Err(rollback)) => Err(format!("{resolved}; {rollback}")),
    }
}

/// Apply the fixture into a fresh schema and hand back the folded expected snapshot.
async fn install(session: &support::PgDevSession, schema: &str) -> Result<SchemaSnapshot, String> {
    let ir = fixture(schema);
    let expected = fold_ops(
        &ir.ops,
        SqlDialect::Postgres,
        schema,
        &support::no_inject(schema),
    )
    .map_err(|error| format!("fold view-body fixture: {error}"))?;
    let migrations = IrAuthor::new(
        schema,
        OWNER,
        SqlDialect::Postgres,
        &support::no_inject(schema),
    )
    .lower(&ir, &LiveSchema::default())
    .map_err(|error| format!("lower view-body fixture: {error}"))?;
    for migration in &migrations {
        session
            .batch(&migration.up)
            .await
            .map_err(|error| format!("apply {}: {error}", migration.name))?;
    }
    Ok(expected)
}

fn require_body_drift(
    drift: &StructuralDrift,
    view: &str,
    expected_contains: &str,
    actual_contains: &str,
) -> Result<(), String> {
    let object = format!("view {view}");
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

/// A body comparison must never disturb the IDENTITY half. If a mutation made the
/// two sides key differently, the body comparison would silently stop running and
/// every clean-schema assertion above it would pass VACUOUSLY.
fn require_view_still_paired(drift: &StructuralDrift, view: &str) -> Result<(), String> {
    let object = format!("view {view}");
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

/// THE MEASUREMENT, kept separate from the comparison it justifies.
///
/// Reads the two view bodies off the two snapshots and asserts each against a
/// literal, so neither representation is normalised into the other by the act of
/// checking. This is the test that says WHY a naive field compare was never
/// available, and it must keep passing after the fix: the fix may not smuggle an
/// `authored_query` onto the introspected side or a `definition` onto the folded
/// one, because a snapshot that claims to carry both would let a later reader
/// believe a catalog can yield a typed body.
#[compio::test]
async fn both_sides_of_a_view_body_are_measured() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token();
    let quoted_schema = quote_ident(&schema);
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated view-body schema");

    let result: Result<(), String> = async {
        let expected = install(&session, &schema).await?;
        let actual = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect clean view-body fixture: {error}"))?;

        let exp_view = expected
            .views
            .get("filtered")
            .ok_or("folded snapshot lost view `filtered`")?;
        let act_view = actual
            .views
            .get("filtered")
            .ok_or("introspected snapshot lost view `filtered`")?;

        // The FOLDED side: a typed body, and no rendered text.
        if exp_view.authored_query.is_none() {
            return Err("folded view carries no `authored_query`".to_string());
        }
        if exp_view.definition.is_some() {
            return Err(format!(
                "folded view unexpectedly carries a rendered `definition`: {:?}",
                exp_view.definition
            ));
        }

        // The INTROSPECTED side: rendered text, and no typed body. A live catalog
        // cannot yield a typed query, and this asserts it still does not.
        if act_view.authored_query.is_some() {
            return Err(
                "introspected view unexpectedly carries a typed `authored_query`".to_string(),
            );
        }

        // And NEITHER side carries a comparable body until `resolve_view_bodies`
        // puts one there. This is what makes the differ's `both Some` guard a real
        // decline rather than a formality: fold and introspection alone leave the
        // body uncompared, which is the pre-existing behaviour every caller that
        // does not run the resolve still gets.
        if exp_view.comparable_body.is_some() || act_view.comparable_body.is_some() {
            return Err(format!(
                "a `comparable_body` appeared without `resolve_view_bodies` running - \
                 expected {:?}, actual {:?}",
                exp_view.comparable_body, act_view.comparable_body
            ));
        }
        let definition = act_view
            .definition
            .as_deref()
            .ok_or("introspected view carries no `definition`")?;

        // And the server's re-print is NOT the authored text: the cast below is the
        // one nobody wrote. Asserted against a literal rather than against the
        // rendered authored body, so the two representations never touch.
        if !definition.contains("::numeric") {
            return Err(format!(
                "expected the server to have inserted a numeric cast the author never \
                 wrote, so that a text compare of the two sides is provably unsound; \
                 catalog body was {definition:?}"
            ));
        }
        Ok(())
    }
    .await;

    session
        .batch(&format!("DROP SCHEMA {quoted_schema} CASCADE"))
        .await
        .expect("drop isolated view-body schema");
    result.expect("view-body representations");
}

/// THE REGRESSION. A `CREATE OR REPLACE VIEW` with the same name and a different
/// body has to be visible, and a schema nobody has touched has to stay clean.
#[compio::test]
async fn live_postgres_reports_view_body_drift() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token();
    let quoted_schema = quote_ident(&schema);
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated view-body schema");

    let result: Result<(), String> = async {
        let expected = install(&session, &schema).await?;

        // THE FALSE-DRIFT CONTROL, and it is the assertion that matters most. The
        // server re-prints every one of these bodies with different whitespace,
        // different quoting and - for `filtered` - an inserted cast. A comparison
        // that could not absorb that would report all four views as drifted
        // immediately, on a schema nobody has touched, which is strictly worse than
        // the blind spot it replaces.
        //
        // This leg runs OUTSIDE a transaction block, which exercises the other half
        // of `resolve_view_bodies`: the `SAVEPOINT` probe fails, it opens and rolls
        // back its own transaction, and the session survives to be used again below.
        let mut clean = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect clean view-body fixture: {error}"))?;
        let mut clean_expected = expected.clone();
        resolve_view_bodies(&session, &schema, &mut clean_expected, &mut clean)
            .await
            .map_err(|error| format!("resolve clean view bodies: {error}"))?;

        // The resolve must actually have RESOLVED something. Without this the whole
        // suite could pass vacuously: a probe that declined every view would leave
        // `definition` `None` on the expected side, the differ would skip the body,
        // and the clean control below would report clean for the wrong reason.
        for view in ["plain_projection", "filtered", "aliased", "replaced"] {
            if clean_expected
                .views
                .get(view)
                .and_then(|v| v.comparable_body.as_ref())
                .is_none()
            {
                return Err(format!(
                    "`resolve_view_bodies` declined view `{view}`, so every body \
                     assertion in this file would pass vacuously"
                ));
            }
        }

        let clean_drift = diff_snapshots(&clean_expected, &clean);
        if !clean_drift.is_clean() {
            return Err(format!("clean view-body fixture drifted: {clean_drift:#?}"));
        }

        for (view, mutation, expected_key, actual_key) in [
            (
                "plain_projection",
                format!(
                    "CREATE OR REPLACE VIEW {quoted_schema}.plain_projection AS \
                     SELECT id, label FROM {quoted_schema}.src WHERE id > 5"
                ),
                "FROM",
                "id > 5",
            ),
            (
                "filtered",
                format!(
                    "CREATE OR REPLACE VIEW {quoted_schema}.filtered AS \
                     SELECT id FROM {quoted_schema}.src WHERE amount > 999"
                ),
                "10",
                "999",
            ),
            (
                "aliased",
                format!(
                    "CREATE OR REPLACE VIEW {quoted_schema}.aliased AS \
                     SELECT s.id AS the_id, upper(s.label) AS label \
                     FROM {quoted_schema}.src s"
                ),
                "label",
                "upper",
            ),
            (
                "replaced",
                format!(
                    "CREATE OR REPLACE VIEW {quoted_schema}.replaced AS \
                     SELECT id, label FROM {quoted_schema}.src WHERE label IS NOT NULL"
                ),
                // The LAST authored body projects id AND label. An expected side
                // holding only `id` here would mean the fold kept the pre-replace
                // body - and the assertion would be measuring the wrong defect.
                "label",
                "IS NOT NULL",
            ),
        ] {
            let (mutated_expected, actual) =
                snapshot_after_mutation(&session, &schema, &expected, &mutation).await?;
            let drift = diff_snapshots(&mutated_expected, &actual);
            require_view_still_paired(&drift, view)?;
            require_body_drift(&drift, view, expected_key, actual_key)?;

            // Only the mutated view may drift. A probe that re-printed the other
            // three under a different `search_path` than the introspection read
            // would report them all, and `require_body_drift` alone would not
            // notice.
            if let Some(other) = drift
                .altered_objects
                .iter()
                .find(|a| a.field == "body" && a.object != format!("view {view}"))
            {
                return Err(format!(
                    "mutation `{mutation}` also reported body drift on an untouched \
                     view: {other:#?}"
                ));
            }
        }
        Ok(())
    }
    .await;

    session
        .batch(&format!("DROP SCHEMA {quoted_schema} CASCADE"))
        .await
        .expect("drop isolated view-body schema");
    result.expect("view-body drift");
}
