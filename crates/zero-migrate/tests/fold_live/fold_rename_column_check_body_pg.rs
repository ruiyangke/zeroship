//! Live PostgreSQL oracle for the CHECK body a column rename leaves in the fold, and
//! for the two consumers that must not read it.
//!
//! ## What this file used to assert, and why that changed
//!
//! It used to pin the fold's CHECK body as deliberately STALE. The `Op::RenameColumn`
//! arm re-rendered `ConstraintSnapshot::definition` for UNIQUE / PRIMARY KEY / FOREIGN
//! KEY and for nothing else, on the grounds that those three open with a LOCAL COLUMN
//! LIST - a closed identifier grammar where a string literal cannot appear - while a
//! CHECK's leading group is an EXPRESSION, where `CHECK ((status <> 'qty'::text))`
//! survives dropping `qty` (measured on PostgreSQL 18.4). Substituting a name inside
//! that text would corrupt the literal, so the arm left the body alone.
//!
//! The trap is real; the conclusion was not. `rename_quoted_column_in_sql` walks a
//! fragment as QUOTED RUNS, so a `'…'` literal is copied through whole and can never be
//! mistaken for a column reference - it was written for `ColumnSnapshot::inline_checks`
//! and it solves the CHECK body identically. The RENAME CARRIER SWEEP applied it, so
//! the fold now projects `CHECK (("amount_on_hand" > 0))` where it used to project
//! `CHECK (("qty_on_hand" > 0))`, and `tests/rename_carrier_sweep_pg.rs` pins the
//! literal's survival beside the reference's move. This file's first assertion is
//! INVERTED rather than relaxed: it now demands the follow it used to forbid.
//!
//! ## What it still asserts, unchanged, and why that is the point
//!
//! The two sides still DIVERGE, because the fold quotes every identifier and
//! `pg_get_constraintdef` does not: the fold says `CHECK (("amount_on_hand" > 0))` and
//! live says `CHECK ((amount_on_hand > 0))`. So every downstream assertion here keeps
//! its subject, and the file keeps its real job - proving that NOTHING READS the folded
//! copy. `apply::drift::constraint_definition_is_comparable` exempts CHECK, so the
//! differ never compares the text; and the existence guard's fail-closed refusal
//! (`render::existence_probe::decide_constraint`, `GuardVerdict::FailDrift` ->
//! `ApplyError::ExistenceGuardDrift`) quotes `actual` out of a snapshot introspected
//! under the held lock inside the guarded transaction, never out of a fold. Neither
//! half is pinned by the fold==live oracles next door, because the differ's exemption
//! makes a CHECK body invisible to a drift comparison by construction.
//!
//! So this test still refuses to take "the differ is quiet" as evidence. It asserts
//! each side of the disagreement separately, so the refusal check below is not
//! comparing two identical strings, and only then drives a guarded
//! `addConstraint ifNotExists` under the SAME constraint name to the refusal a user
//! would read.
//!
//! A failure here means the folded body stopped being private. If the guard is ever
//! wired to a folded snapshot instead of a fresh introspection, its refusal starts
//! quoting a rendering the user's database does not hold.
//!
//! The rename runs as native `ALTER TABLE ... RENAME COLUMN` rather than through the
//! engine's `renameColumn` op - see `drift_between_fold_and_live` in
//! `fold_rename_column_index_cascade_pg.rs` for why.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_create_table_policy, snapshot_schema, Approval,
    EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, PostgresBackend, SchemaSnapshot, SqlDialect,
};

const OWNER: &str = "app_fold_stale_check_body_pg";

/// The pre-rename column name. It appears in NO other identifier this fixture puts in
/// front of the guard - not the table, not the constraint, not a migration name - so
/// the refusal assertion can demand the string is absent from the whole message. The
/// underscores matter: the message also carries the engine-generated `mig_<base58>`
/// version, and an underscore-bearing name cannot collide with that alphabet.
const OLD_COLUMN: &str = "qty_on_hand";

/// The post-rename column name, the one `pg_get_constraintdef` deparses.
const NEW_COLUMN: &str = "amount_on_hand";

const TABLE: &str = "guard_check_rename";
const CONSTRAINT: &str = "guard_check_rename_positive";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_stale_check_body_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// A table carrying a table-level CHECK over the column that is about to be renamed.
const CREATE_TABLE_IR: &str = r#"{
  "ir_version": 1,
  "name": "stale_check_body_create",
  "owner_app": "app_fold_stale_check_body_pg",
  "ops": [
    {"op":"createTable","name":"guard_check_rename","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":false}
    ],"primaryKey":["id"],
     "constraints":[
       {"name":"guard_check_rename_positive","kind":{"kind":"check","expr":{
         "node":"binOp","op":"gt",
         "lhs":{"node":"colRef","name":"qty_on_hand"},
         "rhs":{"node":"literal","value":0}}}}
     ]}
  ]
}"#;

/// The SAME ops plus the rename, folded offline. This is the projection whose CHECK
/// body goes stale.
const FOLDED_IR: &str = r#"{
  "ir_version": 1,
  "name": "stale_check_body_folded",
  "owner_app": "app_fold_stale_check_body_pg",
  "ops": [
    {"op":"createTable","name":"guard_check_rename","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":false}
    ],"primaryKey":["id"],
     "constraints":[
       {"name":"guard_check_rename_positive","kind":{"kind":"check","expr":{
         "node":"binOp","op":"gt",
         "lhs":{"node":"colRef","name":"qty_on_hand"},
         "rhs":{"node":"literal","value":0}}}}
     ]},
    {"op":"renameColumn","table":"guard_check_rename","from":"qty_on_hand","to":"amount_on_hand",
     "type":"int"}
  ]
}"#;

/// Re-add the SAME constraint name under `ifNotExists`, spelling the predicate over
/// the NEW column. The constraint is present, so the guard cannot prove the declared
/// body equal to `pg_get_constraintdef` and must refuse fail-closed.
const GUARDED_READD_IR: &str = r#"{
  "ir_version": 1,
  "name": "stale_check_body_readd",
  "owner_app": "app_fold_stale_check_body_pg",
  "ops": [
    {"op":"addConstraint","table":"guard_check_rename","existenceGuard":"ifNotExists",
     "constraint":{
       "name":"guard_check_rename_positive",
       "kind":{"kind":"check","expr":{
         "node":"binOp","op":"gt",
         "lhs":{"node":"colRef","name":"amount_on_hand"},
         "rhs":{"node":"literal","value":0}}}}}
  ]
}"#;

/// What the run measured: the fold's projected CHECK body, the live catalog's, and
/// the guarded re-add's outcome (`Ok` when the guard let it through, `Err(message)`
/// carrying the `ApplyError` Display a user would read).
struct Measured {
    fold_definition: String,
    live_definition: String,
    fold_versus_live_is_clean: bool,
    fold_versus_live: String,
    guarded_readd: Result<(), String>,
}

/// Look up one constraint's `definition` in a snapshot, or a marker naming what was
/// missing - an absent constraint must read as a failed assertion, never as a body
/// that happens not to contain a column name.
fn constraint_definition(snapshot: &SchemaSnapshot) -> String {
    snapshot
        .tables
        .get(TABLE)
        .and_then(|table| table.constraints.iter().find(|c| c.name == CONSTRAINT))
        .map_or_else(
            || format!("<no constraint {CONSTRAINT} on {TABLE}>"),
            |constraint| constraint.definition.clone(),
        )
}

/// Apply `ir` through the real engine against the real database.
async fn apply_through_engine(
    backend: &PostgresBackend<'_, PgDevSession>,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    ir: &str,
    live: &LiveSchema,
    registry: &BTreeMap<String, String>,
) -> Result<(), String> {
    let authored: MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved IR: {error}"))?;
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
            "fold-stale-check-body-pg",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("{error}"))
}

/// Create the table through the real engine, rename the constrained column with
/// PostgreSQL's own DDL, then read BOTH projections of the CHECK and drive the
/// guarded re-add to whatever the existence guard decides. `None` when no live
/// database is configured (the caller then skips its assertions).
async fn measure() -> Option<Measured> {
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
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated stale-check-body schema");

    let work: Result<Measured, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let policy = support::no_inject(&cfg.project_schema);
        apply_through_engine(
            &backend,
            &cfg,
            &policy,
            CREATE_TABLE_IR,
            &LiveSchema::default(),
            &BTreeMap::new(),
        )
        .await?;

        let rename = format!(
            "ALTER TABLE {quoted_schema}.{} RENAME COLUMN {} TO {}",
            quote_ident(TABLE),
            quote_ident(OLD_COLUMN),
            quote_ident(NEW_COLUMN)
        );
        session
            .batch(&rename)
            .await
            .map_err(|error| format!("run native SQL `{rename}`: {error}"))?;

        let folded_authored: MigrationIr =
            serde_json::from_str(FOLDED_IR).map_err(|error| format!("parse folded IR: {error}"))?;
        let folded_resolved =
            resolve_create_table_policy(&folded_authored, &policy, &cfg.project_schema)
                .map_err(|error| format!("resolve folded create-table policy: {error}"))?;
        let folded = fold_ops(
            &folded_resolved.ops,
            SqlDialect::Postgres,
            &cfg.project_schema,
            &policy,
        )
        .map_err(|error| format!("fold the applied PostgreSQL ops: {error}"))?;
        let live = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
        let drift = diff_snapshots(&folded, &live);

        let live_schema = LiveSchema::from_catalog_snapshot(live.clone(), OWNER);
        let mut registry = BTreeMap::new();
        registry.insert(TABLE.to_string(), OWNER.to_string());
        let guarded_readd = apply_through_engine(
            &backend,
            &cfg,
            &policy,
            GUARDED_READD_IR,
            &live_schema,
            &registry,
        )
        .await;

        Ok(Measured {
            fold_definition: constraint_definition(&folded),
            live_definition: constraint_definition(&live),
            fold_versus_live_is_clean: drift.is_clean(),
            fold_versus_live: format!("{drift:#?}"),
            guarded_readd,
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
        (Ok(measured), Ok(())) => Some(measured),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

#[compio::test]
async fn a_rename_moves_the_folded_check_body_and_nothing_reads_it_anyway() {
    let Some(measured) = measure().await else {
        return;
    };
    let Measured {
        fold_definition,
        live_definition,
        fold_versus_live_is_clean,
        fold_versus_live,
        guarded_readd,
    } = measured;

    // The fold's projected CHECK body FOLLOWS the rename. It is the fourth carrier of
    // the rename carrier sweep, rewritten through the quoted-run walk rather than the
    // column-list re-render the other three constraint kinds use: a CHECK's leading
    // group is an expression, so only a walk that copies string literals through whole
    // can move the reference without corrupting a literal beside it. The anti-corruption
    // half is pinned by `rename_carrier_sweep_pg`, which plants a literal spelling the
    // old column name inside the body and demands it survive byte for byte.
    assert!(
        fold_definition.contains(NEW_COLUMN) && !fold_definition.contains(OLD_COLUMN),
        "the fold's CHECK body must follow `rename {OLD_COLUMN} -> {NEW_COLUMN}`, naming \
         {NEW_COLUMN} and not {OLD_COLUMN}; it read `{fold_definition}`. A folded body \
         naming a column the table no longer has is a snapshot describing a schema that \
         does not exist - and on SQLite it is `CREATE TABLE` DDL that the server refuses"
    );

    // The witness that makes the refusal assertion mean anything: live genuinely
    // disagrees. `pg_constraint.conkey` holds attribute NUMBERS, so
    // `pg_get_constraintdef` deparses the NEW name the instant the rename commits.
    // The two now name the SAME column and still differ, because the fold QUOTES every
    // identifier and the deparser does not - which is what keeps every assertion below
    // pointed at two distinct strings.
    assert!(
        live_definition.contains(NEW_COLUMN) && !live_definition.contains(OLD_COLUMN),
        "PostgreSQL deparses a CHECK over the renamed attribute under its NEW name, so the \
         live body must name {NEW_COLUMN} and not {OLD_COLUMN}; it read `{live_definition}`. \
         With both bodies spelling the same column the refusal assertion below would prove \
         nothing"
    );
    assert_ne!(
        fold_definition, live_definition,
        "the fold and live CHECK bodies must actually diverge for this test to have a \
         subject; both read `{fold_definition}`"
    );

    // First consumer that could expose the staleness: the differ. It exempts CHECK
    // bodies (`constraint_definition_is_comparable`), so a stale body must NOT report.
    assert!(
        fold_versus_live_is_clean,
        "the differ exempts CHECK bodies, so a stale folded body must report NO drift; \
         dropping that exemption turns every rename into permanent drift on the next \
         introspection: {fold_versus_live}"
    );

    // Second consumer: the existence guard. A present same-name constraint whose body
    // cannot be proven equal to `pg_get_constraintdef` is refused fail-closed - an
    // `ifNotExists` op never silently runs over, nor skips, a divergent object.
    let refusal = guarded_readd.err().unwrap_or_else(|| {
        panic!(
            "the guarded `addConstraint ifNotExists` on the already-present constraint \
             {CONSTRAINT} must be REFUSED fail-closed; it applied instead, which means an \
             `ifNotExists` op silently ran over a constraint whose body the guard cannot \
             prove equal to live's `{live_definition}`"
        )
    });

    // The refusal quotes a snapshot introspected inside the guarded transaction, never
    // the fold. This is the assertion that makes the staleness above private.
    assert!(
        refusal.contains(&live_definition),
        "the existence-guard refusal must quote the FRESHLY INTROSPECTED body \
         `{live_definition}`, which is what the user's database actually holds; it read: \
         {refusal}"
    );
    assert!(
        !refusal.contains(&fold_definition),
        "the existence-guard refusal must not quote the fold's rendering \
         `{fold_definition}`; wiring the guard to a folded snapshot makes it tell a user \
         their database holds a body it does not hold. The two agree on the COLUMN now \
         and still differ on QUOTING, which is exactly why this assertion survives the \
         rename-follow that inverted the first one. It read: {refusal}"
    );
    assert!(
        !refusal.contains(OLD_COLUMN),
        "no identifier in this fixture except the pre-rename column spells {OLD_COLUMN}, so \
         a refusal mentioning it can only have come from a projection that missed the \
         rename; it read: {refusal}"
    );
}
