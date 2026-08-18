//! `Op::SetColumnType` against a live PostgreSQL: the half-migration, and the
//! boundary any refusal has to respect.
//!
//! A retype clears `validate`, clears the guard, lowers to a plan and previews
//! that plan - and then, for some companion objects, dies against the server. That
//! by itself would only be a failed migration. It is a HALF migration because a
//! lowered unit commits in its own transaction: an ordinary two-op envelope
//! (`addColumn` then the retype) leaves the added column COMMITTED and the type
//! unchanged, and the operator is left with a schema that is neither the old shape
//! nor the new one.
//!
//! Every arm here drives the REAL path - author -> `load_and_lower_guarded` ->
//! `MigrationEngine::apply_plan`, engine-emitted SQL, no hand-written DDL for the
//! operation under test - and reads the outcome back out of `pg_attribute` and
//! `information_schema` rather than out of the engine's own report.
//!
//! THE CONTROLS ARE HALF THE POINT. The obvious fix - reusing the `dropColumn`
//! dependency assertion - would refuse migrations PostgreSQL honours, and an
//! over-refusal is a defect too. So every blocked arm is paired with an arm whose
//! companion object is one the server accepts, and those must still apply and
//! still commit BOTH ops.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_setcolumntype_half_migration";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "zm_retype_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// What the database looked like after the mutation envelope was attempted.
///
/// Read from the catalog, not from the engine's return value. The engine reporting
/// an error is not evidence that nothing applied - that is precisely the claim
/// under test, and the whole defect is an error report sitting on top of a
/// committed `ADD COLUMN`.
#[derive(Debug)]
struct Outcome {
    /// `Ok(())` if `apply_plan` accepted the mutation envelope, else its rendered
    /// error.
    applied: Result<(), String>,
    /// Whether the envelope's FIRST op (`addColumn survivor`) is present live.
    survivor_present: bool,
    /// The live `data_type` of the retyped column.
    column_type: String,
}

/// Apply `setup` and then `mutation` through the real engine against a live
/// PostgreSQL, and report what the catalog says afterwards.
///
/// `setup` is applied with `apply_plan` too, and is asserted to succeed: a fixture
/// built by the engine proves the shape under test is REACHABLE by an author,
/// which a hand-written `CREATE VIEW` would not.
///
/// `native_setup` is for companions the IR cannot author. It runs after `setup` on
/// the same session. It never carries the operation under test - only its
/// surroundings.
async fn attempt_retype(
    setup: &str,
    native_setup: &[&str],
    mutation: &str,
    retyped_table: &str,
    retyped_column: &str,
) -> Option<Outcome> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated retype schema");

    let work: Result<Outcome, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;
        let policy = support::no_inject(&cfg.project_schema);

        apply_envelope(
            &backend,
            &cfg,
            &policy,
            setup,
            &BTreeMap::new(),
            &LiveSchema::default(),
        )
        .await
        .map_err(|error| format!("the FIXTURE envelope must apply cleanly: {error}"))?;

        for statement in native_setup {
            let statement = statement.replace("{schema}", &quoted_schema);
            session
                .batch(&statement)
                .await
                .map_err(|error| format!("run fixture SQL `{statement}`: {error}"))?;
        }

        // The mutation envelope is a SECOND deploy against a schema the first one
        // built, which is the shape the defect actually appears in. That means it
        // needs what a second deploy really has: the ownership registry, and the
        // live catalog - which is also where the lower reads the retyped column's
        // generation contract from. Handing it `LiveSchema::default()` would make
        // this arm lower a DIFFERENT statement than a real deploy does.
        let snapshot = backend
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the live schema: {error}"))?;
        let live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
        let registry: BTreeMap<String, String> = live
            .table_snapshots
            .keys()
            .map(|table| (table.clone(), OWNER.to_string()))
            .collect();

        let applied = apply_envelope(&backend, &cfg, &policy, mutation, &registry, &live).await;

        let survivor_present = column_exists(&session, &cfg, retyped_table, "survivor").await?;
        let column_type = column_type(&session, &cfg, retyped_table, retyped_column).await?;
        Ok(Outcome {
            applied,
            survivor_present,
            column_type,
        })
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(outcome), Ok(())) => Some(outcome),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

/// Author one IR envelope and drive it through the engine's public apply path.
async fn apply_envelope(
    backend: &PostgresBackend<'_, PgDevSession>,
    cfg: &ExecutorConfig,
    policy: &zero_migrate::EffectivePolicy,
    source: &str,
    registry: &BTreeMap<String, String>,
    live: &LiveSchema,
) -> Result<(), String> {
    let authored: MigrationIr =
        serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved =
        zero_migrate::resolve_create_table_policy(&authored, policy, &cfg.project_schema)
            .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved test IR: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
    // The AUTHORING gate. Every arm below reaches a plan through it, which is what
    // makes "cleared validate, the guard and the lower" a measurement rather than a
    // claim: a defect caught here would never have been the half-migration.
    let artifact = author
        .load_and_lower_guarded(&resolved_source, OWNER, registry, live, &guard)
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            "setcolumntype-half-migration",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("{error}"))
}

async fn column_exists(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let row = session
        .query_one(
            "SELECT EXISTS (
                 SELECT 1 FROM information_schema.columns
                  WHERE table_schema = $1 AND table_name = $2 AND column_name = $3
             ) AS present",
            &[
                cfg.project_schema.as_str().into(),
                table.into(),
                column.into(),
            ],
        )
        .await
        .map_err(|error| format!("read column presence: {error}"))?;
    row.try_get::<_, bool>("present")
        .map_err(|error| format!("decode column presence: {error}"))
}

async fn column_type(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    table: &str,
    column: &str,
) -> Result<String, String> {
    let row = session
        .query_one(
            "SELECT data_type FROM information_schema.columns
              WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[
                cfg.project_schema.as_str().into(),
                table.into(),
                column.into(),
            ],
        )
        .await
        .map_err(|error| format!("read column type: {error}"))?;
    row.try_get::<_, String>("data_type")
        .map_err(|error| format!("decode column type: {error}"))
}

/// The mutation envelope every arm uses: an ordinary op that WOULD commit,
/// followed by the retype. The first op is what turns a failed migration into a
/// half migration, so it is not decoration - it is the instrument.
fn mutation_for(table: &str, column: &str) -> String {
    format!(
        r#"{{
          "ir_version": 1,
          "name": "retype_after_an_op_that_commits",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"addColumn","table":"{table}","column":"survivor","type":"text","nullable":true}},
            {{"op":"setColumnType","table":"{table}","column":"{column}","toType":"bigInt"}}
          ]
        }}"#
    )
}

// ===========================================================================
// The blocked arms: nothing may apply.
// ===========================================================================

/// Case 1: a VIEW reads the column.
///
/// `cannot alter type of a column used by a view or rule`.
#[compio::test]
async fn a_retype_a_view_blocks_applies_nothing_at_all() {
    let setup = format!(
        r#"{{
          "ir_version": 1,
          "name": "retype_view_fixture",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"createTable","name":"vw_src","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"v","type":"int","nullable":true}}
            ],"primaryKey":["id"]}},
            {{"op":"createView","name":"vw_reader","query":{{"kind":"structured","select":{{
              "from":{{"name":"vw_src"}},
              "projection":[{{"kind":"colRef","name":"v"}}]}}}}}}
          ]
        }}"#
    );
    let Some(outcome) =
        attempt_retype(&setup, &[], &mutation_for("vw_src", "v"), "vw_src", "v").await
    else {
        return;
    };
    assert_blocked(&outcome, "rule _RETURN on view");
}

/// Case 2: a GENERATED column reads the column.
///
/// `cannot alter type of a column used by a generated column`. `render/lower.rs`
/// used to carry a comment calling this case "Unreachable through this replay
/// today"; the fixture below is an ordinary `createTable`, so it is reachable.
#[compio::test]
async fn a_retype_a_generated_column_reads_applies_nothing_at_all() {
    let setup = format!(
        r#"{{
          "ir_version": 1,
          "name": "retype_generated_fixture",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"createTable","name":"gen_src","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"v","type":"int","nullable":true}},
              {{"name":"doubled","type":"int","nullable":true,"generated":{{
                "expr":{{"node":"binOp","op":"mul",
                  "lhs":{{"node":"colRef","name":"v"}},
                  "rhs":{{"node":"literal","value":2}}}},
                "stored":true}}}}
            ],"primaryKey":["id"]}}
          ]
        }}"#
    );
    let Some(outcome) =
        attempt_retype(&setup, &[], &mutation_for("gen_src", "v"), "gen_src", "v").await
    else {
        return;
    };
    assert_blocked(&outcome, "default value for column doubled");
}

/// A blocker that is not a dependency at all: the column is part of the table's
/// PARTITION KEY. `cannot alter column "..." because it is part of the partition key
/// of relation "..."`.
///
/// Not in the original three, and found by asking the server about shapes the
/// original measurement did not cover. It matters because no `pg_depend` walk sees
/// it - a dependency-only predicate under-refuses here and the plan half-applies.
#[compio::test]
async fn a_retype_of_a_partition_key_column_applies_nothing_at_all() {
    let setup = format!(
        r#"{{
          "ir_version": 1,
          "name": "retype_partition_key_fixture",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"createTable","name":"pk_src","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"v","type":"int","nullable":false}}
            ],"primaryKey":["id","v"],
             "partitionBy":{{"kind":"range","columns":["v"]}}}}
          ]
        }}"#
    );
    let Some(outcome) =
        attempt_retype(&setup, &[], &mutation_for("pk_src", "v"), "pk_src", "v").await
    else {
        return;
    };
    assert_blocked(&outcome, "partition key of");
}

// ===========================================================================
// The over-refusal controls: these MUST still apply.
// ===========================================================================

/// THE CONTROL THAT DECIDES THE PREDICATE. A column another table's FOREIGN KEY
/// points at BLOCKS a `DROP COLUMN` and does NOT block a retype - measured against
/// the server, and the one row of eight where the drop assertion and the server
/// disagree. Reusing `ColumnHasNoBlockingDependents` here would refuse this
/// migration, and refusing what PostgreSQL honours is a defect, not a safe default.
#[compio::test]
async fn a_retype_of_a_foreign_key_target_column_still_applies() {
    let setup = format!(
        r#"{{
          "ir_version": 1,
          "name": "retype_fk_target_fixture",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"createTable","name":"fk_target","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"v","type":"int","nullable":false,"unique":true}}
            ],"primaryKey":["id"]}},
            {{"op":"createTable","name":"fk_child","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"points_at","type":"int","nullable":true}}
            ],"primaryKey":["id"]}},
            {{"op":"addConstraint","table":"fk_child","constraint":{{
              "name":"fk_child_points_at_fkey",
              "kind":{{"kind":"fk","columns":["points_at"],
                "referencesTable":"fk_target","referencesColumns":["v"]}}}}}}
          ]
        }}"#
    );
    let Some(outcome) = attempt_retype(
        &setup,
        &[],
        &mutation_for("fk_target", "v"),
        "fk_target",
        "v",
    )
    .await
    else {
        return;
    };
    assert_applied(&outcome);
}

/// The rest of the companions PostgreSQL accepts, all on one column at once: a
/// CHECK, a composite UNIQUE, a plain index and a castable DEFAULT. Each was
/// measured accepted alone; together they are the arm that fails if a future
/// predicate widens toward "any dependent blocks".
#[compio::test]
async fn a_retype_with_constraint_shaped_companions_still_applies() {
    let setup = format!(
        r#"{{
          "ir_version": 1,
          "name": "retype_accepted_companions_fixture",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"createTable","name":"ok_src","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"v","type":"int","nullable":false,
                "default":{{"expr":{{"node":"literal","value":0}}}}}}
            ],"primaryKey":["id"],
             "constraints":[
               {{"name":"ok_src_v_positive_check","kind":{{"kind":"check","expr":{{
                 "node":"binOp","op":"ge",
                 "lhs":{{"node":"colRef","name":"v"}},
                 "rhs":{{"node":"literal","value":0}}}}}}}},
               {{"name":"ok_src_id_v_key","kind":{{"kind":"unique","columns":["id","v"]}}}}
             ],
             "indexes":[{{"name":"ok_src_v_idx","columns":[{{"kind":"column","name":"v"}}]}}]}}
          ]
        }}"#
    );
    let Some(outcome) =
        attempt_retype(&setup, &[], &mutation_for("ok_src", "v"), "ok_src", "v").await
    else {
        return;
    };
    assert_applied(&outcome);
}

// ===========================================================================
// The boundary this pass did NOT move.
// ===========================================================================

/// MEASUREMENT, not a fix: an UNCASTABLE DEFAULT is still a half migration.
///
/// `default for column "v" cannot be cast automatically to type bigint`. No
/// dependency walk sees this one - and, measured, no catalog predicate over the
/// column's type can decide it either. Two `text` columns with the same catalog
/// type and the same target get OPPOSITE answers from the server depending only on
/// how the author spelled the default: `DEFAULT 42` is accepted and `DEFAULT '42'`
/// is refused, because the deciding input is the type of the stored default
/// EXPRESSION after `strip_implicit_coercions`, which `pg_attrdef` exposes only as
/// deparsed text.
///
/// This test PINS the gap rather than papering over it. It asserts the current,
/// wrong behaviour, so that whoever fixes it has to come here and change this file
/// deliberately - and so the gap cannot quietly disappear from the record.
#[compio::test]
async fn an_uncastable_default_is_still_a_half_migration() {
    let setup = format!(
        r#"{{
          "ir_version": 1,
          "name": "retype_uncastable_default_fixture",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"createTable","name":"def_src","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"v","type":"text","nullable":true,
                "default":{{"expr":{{"node":"literal","value":"abc"}}}}}}
            ],"primaryKey":["id"]}}
          ]
        }}"#
    );
    let Some(outcome) =
        attempt_retype(&setup, &[], &mutation_for("def_src", "v"), "def_src", "v").await
    else {
        return;
    };

    let error = outcome
        .applied
        .as_ref()
        .expect_err("an uncastable default is still refused by the server");
    assert!(
        error.contains("cannot be cast automatically"),
        "the refusal must still be the SERVER's, reached mid-apply, rather than an \
         engine-side precondition - that difference is the whole gap: {error}"
    );
    assert!(
        outcome.survivor_present,
        "this is the un-fixed case, and the thing that makes it a HALF migration is \
         that the earlier op committed and stayed. If `survivor` is now absent, the \
         gap has been closed and this test should be rewritten as a fixed case rather \
         than deleted"
    );
    assert_eq!(
        outcome.column_type, "text",
        "the retype itself must not have taken effect"
    );
}

// ===========================================================================

/// A blocked retype leaves the database EXACTLY as it was: the earlier op did not
/// commit, and the type did not change.
fn assert_blocked(outcome: &Outcome, expected_blocker: &str) {
    let error = outcome
        .applied
        .as_ref()
        .expect_err("the plan must be refused rather than half-applied");
    // Asserted BEFORE the wording check on purpose. This is the claim the whole
    // fix is about, and it is the one whose failure has to be legible: with the
    // precondition disabled every arm here reports exactly this line, beside the
    // server's own sentence.
    assert!(
        !outcome.survivor_present,
        "the envelope's FIRST op committed and stayed, so the schema is neither the \
         old shape nor the new one - this is the half migration itself, not a \
         near-miss: {error}"
    );
    assert_eq!(
        outcome.column_type, "integer",
        "the column type must be untouched"
    );
    assert!(
        error.contains(expected_blocker),
        "the refusal must name what the database says blocks the retype, in the \
         server's own wording; expected to find {expected_blocker:?} in: {error}"
    );
}

/// An accepted retype applies BOTH ops. Asserting only that it did not error would
/// pass against an engine that silently skipped the plan.
fn assert_applied(outcome: &Outcome) {
    outcome
        .applied
        .as_ref()
        .expect("PostgreSQL accepts this retype, so the engine must not refuse it");
    assert!(
        outcome.survivor_present,
        "the envelope's first op must have committed"
    );
    assert_eq!(
        outcome.column_type, "bigint",
        "the retype must actually have taken effect, not merely not-errored"
    );
}
