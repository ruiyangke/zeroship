//! Live PostgreSQL coverage for the PLAN-WIDE live precondition preflight.
//!
//! The engine's atomicity boundary is the STEP, not the plan: one lowered unit is
//! `BEGIN; <up>; INSERT journal; COMMIT`, so a plan of several units commits
//! several times. Every precondition, however, is evaluated inside the
//! per-migration loop - by which time an earlier unit has already committed. A
//! live-database check therefore cannot prevent a half-applied PLAN, only a
//! half-applied MIGRATION.
//!
//! Every arm here drives the REAL path - author -> `load_and_lower_guarded` ->
//! `MigrationEngine::apply_plan`, engine-emitted SQL, no hand-written DDL for the
//! operation under test - and reads the verdict back out of `information_schema`
//! rather than out of the engine's own report. An error return is not evidence
//! that nothing applied; that is exactly the claim under test.
//!
//! THE OVER-REFUSAL CONTROLS ARE HALF THE POINT. A later step's precondition may
//! legitimately depend on state an EARLIER step of the same plan creates or
//! removes - `[dropView, dropColumn]` is the ordinary case, where the view the
//! catalog reports as a blocker is removed by the plan itself one step earlier.
//! Hoisting such an assertion naively REFUSES A VALID PLAN, which is a defect and
//! not a safe default. So every refusal arm is paired with a plan of the same
//! shape that must still apply and must still commit BOTH ops.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_plan_precondition_preflight";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_nanos();
    format!(
        "zm_planpre_{}_{}_{}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// What the database looked like after the mutation envelope was attempted.
#[derive(Debug)]
struct Outcome {
    /// `Ok(())` if `apply_plan` accepted the envelope, else its rendered error.
    applied: Result<(), String>,
    /// Whether the envelope's FIRST op left a committed residue behind. For the
    /// refusal arms this is `addColumn survivor`; a `true` here IS the
    /// half-migration.
    survivor_present: bool,
    /// Whether the column the last op targets is still live.
    target_column_present: bool,
    /// The live `data_type` of the target column, or `"<absent>"`.
    target_column_type: String,
    /// Whether the fixture view is still live.
    view_present: bool,
}

/// Apply `setup` and then `mutation` through the real engine against a live
/// PostgreSQL, and report what the catalog says afterwards.
async fn attempt(
    setup: &str,
    mutation: &str,
    target_table: &str,
    target_column: &str,
    view: &str,
) -> Option<Outcome> {
    attempt_under(
        setup,
        mutation,
        target_table,
        target_column,
        view,
        support::no_inject,
    )
    .await
}

/// [`attempt`] with the charter chosen by the caller.
///
/// A vendor primitive's authority at lower is the charter's capability grant, so an
/// arm whose fixture emits one - `createView materialized` needs
/// `code.materialized_view` - has to author it. Every other arm keeps `no_inject`,
/// so widening the charter here cannot quietly widen theirs.
async fn attempt_under(
    setup: &str,
    mutation: &str,
    target_table: &str,
    target_column: &str,
    view: &str,
    policy_for: fn(&str) -> zero_migrate::EffectivePolicy,
) -> Option<Outcome> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = policy_for(&schema);
    let mut cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    cfg.confinement.meta_schema = format!("meta_{schema}");
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.confinement.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated preflight schema");

    let work: Result<Outcome, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;
        let policy = policy_for(&cfg.project_schema);

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

        // The mutation envelope is a SECOND deploy against a schema the first one
        // built, which is the shape the defect actually appears in.
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

        let survivor_present = column_exists(&session, &cfg, target_table, "survivor").await?;
        let target_column_present =
            column_exists(&session, &cfg, target_table, target_column).await?;
        let target_column_type = column_type(&session, &cfg, target_table, target_column).await?;
        let view_present = relation_exists(&session, &cfg, view).await?;
        Ok(Outcome {
            applied,
            survivor_present,
            target_column_present,
            target_column_type,
            view_present,
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
    let artifact = author
        .load_and_lower_guarded(&resolved_source, OWNER, registry, live, &guard)
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            "plan-precondition-preflight",
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

/// Whether a relation of ANY kind is live in the project schema.
///
/// Read from `pg_class`, NOT from `information_schema.tables`. PostgreSQL omits
/// materialized views from `information_schema` entirely, so the
/// `information_schema` form this replaces reports a committed matview as ABSENT -
/// which is a residue probe that fails toward "nothing was left behind", the exact
/// direction a half-migration test must never fail in. Measured against the live
/// server before this was changed: `information_schema.tables` = 0, `pg_class` = 1
/// for the same matview.
async fn relation_exists(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    relation: &str,
) -> Result<bool, String> {
    let row = session
        .query_one(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = $1 AND c.relname = $2
             ) AS present",
            &[cfg.project_schema.as_str().into(), relation.into()],
        )
        .await
        .map_err(|error| format!("read relation presence: {error}"))?;
    row.try_get::<_, bool>("present")
        .map_err(|error| format!("decode relation presence: {error}"))
}

async fn column_type(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    table: &str,
    column: &str,
) -> Result<String, String> {
    let rows = session
        .query(
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
    match rows.first() {
        None => Ok("<absent>".to_string()),
        Some(row) => row
            .try_get::<_, String>("data_type")
            .map_err(|error| format!("decode column type: {error}")),
    }
}

/// A table whose column a view reads - the catalog blocker both the drop and the
/// retype assertions report.
fn fixture(table: &str, view: &str, column: &str) -> String {
    format!(
        r#"{{
          "ir_version": 1,
          "name": "plan_preflight_fixture",
          "owner_app": "{OWNER}",
          "ops": [
            {{"op":"createTable","name":"{table}","columns":[
              {{"name":"id","type":"int","nullable":false}},
              {{"name":"{column}","type":"int","nullable":true}}
            ],"primaryKey":["id"]}},
            {{"op":"createView","name":"{view}","query":{{"kind":"structured","select":{{
              "from":{{"name":"{table}"}},
              "projection":[{{"kind":"colRef","name":"{column}"}}]}}}}}}
          ]
        }}"#
    )
}

// ===========================================================================
// The refusal arms: an earlier step must not commit.
// ===========================================================================

/// A `dropColumn` the database will refuse, behind an op that WOULD commit.
///
/// `Op::DropColumn` stamps `ColumnHasNoBlockingDependents`, which is evaluated in
/// the per-migration loop - after the `addColumn` unit has already committed. The
/// residue is read from `information_schema`, not from the engine's report.
#[compio::test]
async fn a_blocked_drop_behind_a_committing_op_leaves_nothing_behind() {
    let Some(outcome) = attempt(
        &fixture("dropsrc", "dropsrc_reader", "doomed"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "drop_behind_a_committing_op",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"addColumn","table":"dropsrc","column":"survivor","type":"text","nullable":true}},
                {{"op":"dropColumn","table":"dropsrc","column":"doomed"}}
              ]
            }}"#
        ),
        "dropsrc",
        "doomed",
        "dropsrc_reader",
    )
    .await
    else {
        return;
    };

    let error = outcome
        .applied
        .expect_err("a view reads the column, so the drop must be refused");
    // THE FIRING POINT MOVES; THE READING DOES NOT. This is the same
    // `PreconditionFailed` the per-migration seam builds, for the same version,
    // with the same `is unmet (OnUnmet::Halt): blocking dependents [...]` text
    // and the same `pg_describe_object` wording for the blocker. Only WHEN it
    // arrives is different - and with it, whether the earlier step has committed.
    assert!(
        error.contains("ColumnHasNoBlockingDependents")
            && error.contains("doomed")
            && error.contains("is unmet (OnUnmet::Halt)")
            && error.contains("blocking dependents")
            && error.contains("dropsrc_reader"),
        "the plan-level refusal must read exactly as the per-migration one does, \
         naming the assertion, the column and what the catalog says blocks it: {error}"
    );
    assert!(
        outcome.target_column_present,
        "the refused drop must leave its own column alone"
    );
    assert!(
        !outcome.survivor_present,
        "THE HALF MIGRATION: `addColumn survivor` committed before the drop was \
         refused, leaving a schema that is neither the old shape nor the new one"
    );
}

/// **The capability the op-derived prefix test adds, adjudicated by the server.**
///
/// `CREATE MATERIALIZED VIEW` is a shape this engine really emits and which
/// provably clears no obstruction: it creates a NEW relation, and PostgreSQL has no
/// `CREATE OR REPLACE MATERIALIZED VIEW` at all - the renderer refuses
/// `materialized + replace` fail-closed - so it can never be the replacing leg that
/// silently recomputes another object's dependency edges.
///
/// The SQL whitelist this replaces could not see that. `CREATE MATERIALIZED VIEW`
/// parses as a `CreateTableAsStmt`, which was outside the list, so it answered "may
/// clear an obstruction", the hoist was disarmed, and the plan half-applied. It was
/// one of five renderer-emitted additive shapes measured to answer that way on
/// `839b9aca`.
///
/// THE ORACLE IS THE SERVER, AND IT IS ASSERTED FIRST. Before anything is claimed
/// about the engine, this drives a matview and then the blocked drop through raw
/// SQL on the live connection and reads PostgreSQL's own verdict: the drop is still
/// refused, and the `DETAIL` names only the ordinary view. That is what "the matview
/// clears nothing" MEANS, and it is measured rather than reasoned about.
#[compio::test]
async fn a_drop_behind_a_created_matview_leaves_nothing_behind() {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };

    // ---- STAGE 1: the external oracle. PostgreSQL's answer, before the engine's.
    {
        let session = PgDevSession::connect(&url);
        let schema = token();
        let quoted = quote_ident(&schema);
        let _guard = support::SchemaGuard::arm(&session, [schema.clone()]);
        session
            .batch(&format!(
                "CREATE SCHEMA {quoted}; \
                 CREATE TABLE {quoted}.\"t\" (\"id\" int PRIMARY KEY, \"doomed\" int); \
                 CREATE VIEW {quoted}.\"reader\" AS SELECT \"doomed\" FROM {quoted}.\"t\"; \
                 CREATE MATERIALIZED VIEW {quoted}.\"mv\" AS SELECT \"id\" FROM {quoted}.\"t\""
            ))
            .await
            .expect("the oracle fixture must build");
        session
            .batch(&format!(
                "ALTER TABLE {quoted}.\"t\" DROP COLUMN \"doomed\""
            ))
            .await
            .expect_err("the ordinary view still blocks the drop AFTER the matview ran");

        // WHICH objects block it, read straight out of `pg_depend`/`pg_rewrite`
        // rather than from the engine's own predicate - an engine-authored query
        // here would be the artifact grading its own homework.
        let blockers = session
            .query(
                "SELECT DISTINCT dependent.relname AS name
                   FROM pg_depend d
                   JOIN pg_rewrite r ON r.oid = d.objid
                   JOIN pg_class dependent ON dependent.oid = r.ev_class
                   JOIN pg_class src ON src.oid = d.refobjid
                   JOIN pg_namespace n ON n.oid = src.relnamespace
                   JOIN pg_attribute a
                     ON a.attrelid = src.oid AND a.attnum = d.refobjsubid
                  WHERE n.nspname = $1 AND src.relname = $2 AND a.attname = $3
                    AND dependent.oid <> src.oid",
                &[schema.as_str().into(), "t".into(), "doomed".into()],
            )
            .await
            .expect("read the blocker set out of pg_depend");
        let names: Vec<String> = blockers
            .iter()
            .map(|row| row.try_get::<_, String>("name").expect("decode relname"))
            .collect();
        assert!(
            names.iter().any(|n| n == "reader"),
            "the ordinary view must be in the blocker set: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "mv"),
            "THE ORACLE: the matview must NOT be in the blocker set. It reads a \
             different column, so creating it neither adds nor removes a blocker on \
             `doomed` - which is what makes hoisting the drop's assertion behind it \
             correct rather than merely convenient: {names:?}"
        );
        session
            .batch(&format!("DROP SCHEMA {quoted} CASCADE"))
            .await
            .expect("drop the oracle schema");
    }

    // ---- STAGE 2: the engine, against the answer the server just gave.
    let Some(outcome) = attempt_under(
        &fixture("mvsrc", "mvsrc_reader", "doomed"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "drop_behind_a_created_matview",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"createView","name":"mvsrc_residue","materialized":true,
                  "query":{{"kind":"structured","select":{{
                    "from":{{"name":"mvsrc"}},
                    "projection":[{{"kind":"colRef","name":"id"}}]}}}}}},
                {{"op":"dropColumn","table":"mvsrc","column":"doomed"}}
              ]
            }}"#
        ),
        "mvsrc",
        "doomed",
        // The residue probe slot, read from `pg_class` so a committed matview is
        // actually visible to it.
        "mvsrc_residue",
        support::operator_charter,
    )
    .await
    else {
        return;
    };

    let error = outcome
        .applied
        .expect_err("a view reads the column, so the drop must still be refused");
    assert!(
        error.contains("ColumnHasNoBlockingDependents") && error.contains("doomed"),
        "the refusal must still read as the per-migration one does: {error}"
    );
    assert!(
        outcome.target_column_present,
        "the refused drop must leave its own column alone"
    );
    assert!(
        !outcome.view_present,
        "THE HALF MIGRATION THIS CLOSES: the matview must NOT have committed. The \
         server says it clears no obstruction, so the drop's assertion is answerable \
         before the plan starts and the whole plan is refused with nothing applied. \
         A residue here is the SQL whitelist's over-conservative verdict coming back"
    );
}

/// The SAME refusal for a ONE-step plan, where nothing is hoisted at all.
///
/// A single `dropColumn` has no earlier step, so its assertion is never plan-
/// stable and the refusal still comes from the per-migration seam. Pinning the
/// two side by side is what makes "the firing point moves, the reading does not"
/// a measurement rather than a claim.
#[compio::test]
async fn a_lone_blocked_drop_reports_what_it_always_did() {
    let Some(outcome) = attempt(
        &fixture("lonedrop", "lonedrop_reader", "doomed"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "a_lone_blocked_drop",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"dropColumn","table":"lonedrop","column":"doomed"}}
              ]
            }}"#
        ),
        "lonedrop",
        "doomed",
        "lonedrop_reader",
    )
    .await
    else {
        return;
    };

    let error = outcome
        .applied
        .expect_err("a view reads the column, so the drop must be refused");
    assert!(
        error.contains("ColumnHasNoBlockingDependents")
            && error.contains("doomed")
            && error.contains("is unmet (OnUnmet::Halt)")
            && error.contains("blocking dependents")
            && error.contains("lonedrop_reader"),
        "a one-step plan still refuses at the per-migration seam, in the same \
         words the plan-level phase now uses: {error}"
    );
    assert!(outcome.target_column_present);
}

// ===========================================================================
// The over-refusal controls: a valid plan must still apply.
// ===========================================================================

/// The blocker is removed by the SAME plan, one step earlier.
///
/// `[dropView, dropColumn]` is the ordinary way an author retires a column and
/// the view that reads it. Against the PRE-PLAN database the drop's
/// `ColumnHasNoBlockingDependents` assertion is UNMET - the view is still there -
/// and it becomes met only because step 1 removes it. A plan-level hoist that
/// does not understand that refuses a plan PostgreSQL honours.
#[compio::test]
async fn a_drop_whose_blocker_the_same_plan_removes_still_applies() {
    let Some(outcome) = attempt(
        &fixture("repairdrop", "repairdrop_reader", "doomed"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "drop_after_removing_the_blocker",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"dropView","name":"repairdrop_reader"}},
                {{"op":"dropColumn","table":"repairdrop","column":"doomed"}}
              ]
            }}"#
        ),
        "repairdrop",
        "doomed",
        "repairdrop_reader",
    )
    .await
    else {
        return;
    };

    outcome
        .applied
        .expect("the plan removes the blocker itself, so it must apply");
    assert!(
        !outcome.view_present,
        "the view the plan dropped must be gone"
    );
    assert!(
        !outcome.target_column_present,
        "the column the plan dropped must be gone"
    );
}

/// The same shape for the RETYPE assertion, which is ALREADY hoisted on `main`.
///
/// `preflight_plan_column_retypes` asks `ColumnTypeChangeHasNoBlockers` against
/// the PRE-PLAN database with no reading of what the plan itself does, so
/// `[dropView, setColumnType]` is refused today even though every step of it
/// succeeds when run.
#[compio::test]
async fn a_retype_whose_blocker_the_same_plan_removes_still_applies() {
    let Some(outcome) = attempt(
        &fixture("repairretype", "repairretype_reader", "widened"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "retype_after_removing_the_blocker",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"dropView","name":"repairretype_reader"}},
                {{"op":"setColumnType","table":"repairretype","column":"widened","toType":"bigInt"}}
              ]
            }}"#
        ),
        "repairretype",
        "widened",
        "repairretype_reader",
    )
    .await
    else {
        return;
    };

    outcome
        .applied
        .expect("the plan removes the blocker itself, so it must apply");
    assert!(
        !outcome.view_present,
        "the view the plan dropped must be gone"
    );
    assert_eq!(
        outcome.target_column_type, "bigint",
        "the retype the plan asked for must have landed"
    );
}

/// The blocker is REDEFINED rather than dropped, which is the canonical way to
/// retire a column a view reads.
///
/// `CREATE OR REPLACE VIEW` recomputes the view's dependency edges, so a body
/// that stops selecting the column removes that column's blocker with no `DROP`
/// anywhere in the plan. A prefix test that asks "does this statement drop
/// something" reads this as a creation and hoists - and refuses a plan the
/// server honours. Asking instead "is this one of the shapes proven to add only"
/// gets it right, because the additive answer has to look at `replace`.
#[compio::test]
async fn a_drop_behind_a_replaced_view_still_applies() {
    let Some(outcome) = attempt(
        &fixture("replacedview", "replacedview_reader", "doomed"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "drop_after_redefining_the_view",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"createView","name":"replacedview_reader","replace":true,
                  "columns":["doomed"],
                  "query":{{"kind":"structured","select":{{
                    "from":{{"name":"replacedview"}},
                    "projection":[{{"kind":"colRef","name":"id"}}]}}}}}},
                {{"op":"dropColumn","table":"replacedview","column":"doomed"}}
              ]
            }}"#
        ),
        "replacedview",
        "doomed",
        "replacedview_reader",
    )
    .await
    else {
        return;
    };

    outcome
        .applied
        .expect("the plan redefines the view off the column, so it must apply");
    assert!(
        outcome.view_present,
        "the redefined view must still be there"
    );
    assert!(
        !outcome.target_column_present,
        "the column the plan dropped must be gone"
    );
}

/// A plan every step of which is already journaled must still be a clean no-op,
/// even once the world has moved on underneath it.
///
/// The executor's pending set is `set - completed - superseded`, so a completed
/// migration's preconditions are never evaluated. A plan-level phase that reads
/// every step regardless is therefore not the same question asked earlier - it
/// is a NEW gate on a step this run will not run. The single-variant retype
/// preflight this work replaces did exactly that, and the result is a plan
/// refused FOREVER: deploy the retype, then add a view over the retyped column
/// (which is legal, and which the retype's own dependents commonly are), and
/// every subsequent re-run of that deploy dies on a blocker for a step that
/// completed long ago.
#[compio::test]
async fn a_replayed_plan_is_not_re_judged_against_a_world_that_moved() {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let mut cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    cfg.confinement.meta_schema = format!("meta_{schema}");
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.confinement.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated replay schema");

    let work: Result<Result<(), String>, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;
        let policy = support::no_inject(&cfg.project_schema);

        let setup = format!(
            r#"{{
              "ir_version": 1,
              "name": "replay_fixture",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"createTable","name":"replayed","columns":[
                  {{"name":"id","type":"int","nullable":false}},
                  {{"name":"widened","type":"int","nullable":true}}
                ],"primaryKey":["id"]}}
              ]
            }}"#
        );
        apply_envelope(
            &backend,
            &cfg,
            &policy,
            &setup,
            &BTreeMap::new(),
            &LiveSchema::default(),
        )
        .await
        .map_err(|error| format!("the FIXTURE envelope must apply cleanly: {error}"))?;

        let mutation = format!(
            r#"{{
              "ir_version": 1,
              "name": "replayed_retype",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"addColumn","table":"replayed","column":"survivor","type":"text","nullable":true}},
                {{"op":"setColumnType","table":"replayed","column":"widened","toType":"bigInt"}}
              ]
            }}"#
        );

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
        apply_envelope(&backend, &cfg, &policy, &mutation, &registry, &live)
            .await
            .map_err(|error| format!("the first deploy of the retype must apply: {error}"))?;

        // The world moves on: something reads the retyped column. That is an
        // ordinary thing for a deploy to do, and it is now a blocker the retype's
        // own assertion would report.
        session
            .batch(&format!(
                "CREATE VIEW {quoted_schema}.\"replayed_reader\" AS \
                 SELECT \"widened\" FROM {quoted_schema}.\"replayed\""
            ))
            .await
            .map_err(|error| format!("create the after-the-fact reader: {error}"))?;

        // Re-run the SAME deploy. Every step is journaled completed, so the
        // executor applies nothing.
        let snapshot = backend
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("re-snapshot the live schema: {error}"))?;
        let live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
        let registry: BTreeMap<String, String> = live
            .table_snapshots
            .keys()
            .map(|table| (table.clone(), OWNER.to_string()))
            .collect();
        Ok(apply_envelope(&backend, &cfg, &policy, &mutation, &registry, &live).await)
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    let replay = match (work, cleanup) {
        (Ok(replay), Ok(())) => replay,
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    };
    replay.expect(
        "every step of the replayed plan is journaled completed and would not be \
         evaluated, so nothing may refuse it",
    );
}

/// The SAME prefix test covers the online rename's plan-level dependents check.
///
/// An online rename ends by dropping the old column, so it asks the drop
/// question up front for exactly the reason the drop's own assertion is now
/// asked up front. It is the same OBSTRUCTION question, so it needs the same
/// reading of the plan: `[dropView, renameColumn]` removes its own blocker one
/// step earlier and must apply.
#[compio::test]
async fn a_rename_whose_blocker_the_same_plan_removes_still_applies() {
    let Some(outcome) = attempt(
        &fixture("repairrename", "repairrename_reader", "qty"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "rename_after_removing_the_blocker",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"dropView","name":"repairrename_reader"}},
                {{"op":"renameColumn","table":"repairrename","from":"qty","to":"amount",
                  "type":"int"}}
              ]
            }}"#
        ),
        "repairrename",
        "amount",
        "repairrename_reader",
    )
    .await
    else {
        return;
    };

    outcome
        .applied
        .expect("the plan removes the blocker itself, so it must apply");
    assert!(
        !outcome.view_present,
        "the view the plan dropped must be gone"
    );
}

/// A later step's assertion about a column an EARLIER step of the same plan adds.
///
/// The drop's assertion names `fresh`, which does not exist against the pre-plan
/// database at all. Nothing may refuse the plan on that basis.
#[compio::test]
async fn a_drop_of_a_column_the_same_plan_adds_still_applies() {
    let Some(outcome) = attempt(
        &fixture("addthendrop", "addthendrop_reader", "kept"),
        &format!(
            r#"{{
              "ir_version": 1,
              "name": "add_then_drop_in_one_plan",
              "owner_app": "{OWNER}",
              "ops": [
                {{"op":"addColumn","table":"addthendrop","column":"survivor","type":"text","nullable":true}},
                {{"op":"addColumn","table":"addthendrop","column":"fresh","type":"text","nullable":true}},
                {{"op":"dropColumn","table":"addthendrop","column":"fresh"}}
              ]
            }}"#
        ),
        "addthendrop",
        "fresh",
        "addthendrop_reader",
    )
    .await
    else {
        return;
    };

    outcome
        .applied
        .expect("adding a column and dropping it in one plan is valid");
    assert!(
        outcome.survivor_present,
        "the unrelated addColumn must have committed"
    );
    assert!(
        !outcome.target_column_present,
        "the column the plan added and then dropped must be gone"
    );
}
