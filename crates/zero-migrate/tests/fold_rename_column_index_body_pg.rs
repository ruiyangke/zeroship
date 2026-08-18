//! Live PostgreSQL oracle for the two RENDERED-SQL index bodies a column rename has to
//! follow, and for the differ exemption that still covers their SPELLING.
//!
//! ## What this file used to assert, and why that changed
//!
//! It used to pin the partial-index `predicate` and the text inside an
//! `IndexElementSnapshot::Expr` key as deliberately STALE. The `Op::RenameColumn` arm
//! rewrote every STRUCTURED column name an index carries - `IndexSnapshot::columns`,
//! the `Column` variant of `elements`, the `INCLUDE` payload and the
//! `expr_cascade_columns` provenance - and neither rendered-SQL site, on the grounds
//! that repairing them needed the closed `Expr` the snapshot discarded and that
//! substituting a name inside rendered SQL is the false positive the provenance exists
//! to avoid (`WHERE (note <> 'a')` would become `WHERE (note <> 'b')`).
//!
//! The trap is real; the conclusion was not. `rename_quoted_column_in_sql` walks a
//! fragment as QUOTED RUNS, so a `'…'` literal is copied through whole and the false
//! positive is structurally unreachable - and the fold renders both bodies through
//! `render_expr_inline`, which quotes every `ColRef` with the same speller that walk's
//! round-trip guard re-quotes with. The RENAME CARRIER SWEEP applied it to both sites,
//! so the fold now projects `("amount_on_hand" > 0)` and `("amount_on_hand" + 1)`. The
//! two `must stay stale` assertions below are INVERTED rather than relaxed.
//!
//! What is still left stale, and stated so it is not mistaken for completeness: a
//! CATALOG-derived body. `pg_get_indexdef` deparses its identifiers BARE, the walk
//! matches only QUOTED ones, and introspection recovers no AST to re-render from - so a
//! snapshot projected onto a live base keeps the old spelling. That is the
//! stale-not-corrupt line this crate draws everywhere else, and the differ exemption
//! below is what keeps it from reporting.
//!
//! ## What it still asserts, unchanged
//!
//! PostgreSQL holds `indpred` / `indexprs` as parse trees over ATTRIBUTE NUMBERS, so
//! `pg_get_indexdef` deparses the NEW name the instant the rename commits. Measured
//! here on PostgreSQL 18.4, the two sides still DIVERGE - the fold QUOTES every
//! identifier and the deparser does not - so `("amount_on_hand" > 0)` meets
//! `(amount_on_hand > 0)` and every assertion below keeps two distinct strings to
//! compare.
//!
//! The differ used to byte-compare both, so that disagreement reported as drift on
//! every introspection, forever, and no apply could clear it. It no longer compares
//! either BODY (`apply::drift::index_expression_bodies_are_comparable`) and compares
//! the REFERENCED-COLUMN SET instead, recovered structurally from `pg_depend` on the
//! live side. This test refuses to take "the differ is quiet" as evidence: it asserts
//! each side of the disagreement separately and only then asserts what the differ says
//! about the pair.
//!
//! The last two arms are what keep the exemption from being a capitulation. A fold
//! whose predicate and expression key read a DIFFERENT column than live's must still
//! report, and a fold missing the predicate live carries must still report. Without
//! them a differ that simply stopped looking at partial indexes would pass.
//!
//! What is conceded, and it is a real loss: two expressions over the SAME columns with
//! DIFFERENT logic compare equal. `WHERE (qty > 0)` and `WHERE (qty > -2147483648)`
//! are indistinguishable here, exactly as they already are for a CHECK body.
//!
//! The rename runs as native `ALTER TABLE ... RENAME COLUMN` rather than through the
//! engine's `renameColumn` op - see `drift_between_fold_and_live` in
//! `fold_rename_column_index_cascade_pg.rs` for why.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_create_table_policy, snapshot_schema, Approval,
    EffectivePolicy, ExecutorConfig, GuardConfig, IndexElementSnapshot, IrAuthor, LiveSchema,
    LockMode, MigrationEngine, MigrationIr, PostgresBackend, SchemaSnapshot, SqlDialect,
};

const OWNER: &str = "app_fold_stale_index_body_pg";

/// The pre-rename column name, the one both rendered bodies keep naming.
const OLD_COLUMN: &str = "qty_on_hand";

/// The post-rename column name, the one `pg_get_indexdef` deparses.
const NEW_COLUMN: &str = "amount_on_hand";

const TABLE: &str = "idx_body_rename";
const PARTIAL_INDEX: &str = "idx_body_rename_partial";
const EXPR_INDEX: &str = "idx_body_rename_expr";

/// The drift field the referenced-column set reports under. Spelled here so the
/// coverage arm below fails when the compare is renamed away rather than passing on
/// some other index facet that happened to differ.
const REFERENCED_COLUMNS_FIELD: &str = "referenced_columns";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_stale_index_body_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// A partial index whose `WHERE` reads the column about to be renamed, plus an index
/// KEYED on an expression over the same column. Both bodies are rendered SQL text.
const CREATE_IR: &str = r#"{
  "ir_version": 1,
  "name": "stale_index_body_create",
  "owner_app": "app_fold_stale_index_body_pg",
  "ops": [
    {"op":"createTable","name":"idx_body_rename","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":true},
      {"name":"note","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_partial",
     "columns":[{"kind":"column","name":"id"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"qty_on_hand"},
              "rhs":{"node":"literal","value":0}}},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"qty_on_hand"},"rhs":{"node":"literal","value":1}}}],
     "unique":false}
  ]
}"#;

/// The SAME ops plus the rename, folded offline. This is the projection whose two
/// rendered bodies go stale.
const FOLDED_IR: &str = r#"{
  "ir_version": 1,
  "name": "stale_index_body_folded",
  "owner_app": "app_fold_stale_index_body_pg",
  "ops": [
    {"op":"createTable","name":"idx_body_rename","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":true},
      {"name":"note","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_partial",
     "columns":[{"kind":"column","name":"id"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"qty_on_hand"},
              "rhs":{"node":"literal","value":0}}},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"qty_on_hand"},"rhs":{"node":"literal","value":1}}}],
     "unique":false},
    {"op":"renameColumn","table":"idx_body_rename","from":"qty_on_hand",
     "to":"amount_on_hand","type":"int"}
  ]
}"#;

/// The kept-coverage witness. Identical to `FOLDED_IR` except that BOTH expression
/// sites read `id` instead of the renamed column, so the referenced-column set really
/// differs from live's while the shapes (element count, order, kinds, predicate
/// presence) stay identical. Only a set comparison can see this.
const WRONG_COLUMN_IR: &str = r#"{
  "ir_version": 1,
  "name": "stale_index_body_wrong_column",
  "owner_app": "app_fold_stale_index_body_pg",
  "ops": [
    {"op":"createTable","name":"idx_body_rename","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":true},
      {"name":"note","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_partial",
     "columns":[{"kind":"column","name":"id"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"id"},
              "rhs":{"node":"literal","value":0}}},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"id"},"rhs":{"node":"literal","value":1}}}],
     "unique":false},
    {"op":"renameColumn","table":"idx_body_rename","from":"qty_on_hand",
     "to":"amount_on_hand","type":"int"}
  ]
}"#;

/// The second kept-coverage witness. Identical to `FOLDED_IR` except that the partial
/// index carries NO predicate at all, so `None` meets live's `Some`. Presence is not
/// part of the exemption.
const NO_PREDICATE_IR: &str = r#"{
  "ir_version": 1,
  "name": "stale_index_body_no_predicate",
  "owner_app": "app_fold_stale_index_body_pg",
  "ops": [
    {"op":"createTable","name":"idx_body_rename","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":true},
      {"name":"note","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_partial",
     "columns":[{"kind":"column","name":"id"}],"unique":false},
    {"op":"createIndex","table":"idx_body_rename","name":"idx_body_rename_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"qty_on_hand"},"rhs":{"node":"literal","value":1}}}],
     "unique":false},
    {"op":"renameColumn","table":"idx_body_rename","from":"qty_on_hand",
     "to":"amount_on_hand","type":"int"}
  ]
}"#;

/// What the run measured: both rendered bodies as each side spells them, and the
/// differ's verdict on the stale pair plus on the two witnesses that must still report.
struct Measured {
    fold_predicate: String,
    live_predicate: String,
    fold_expr_element: String,
    live_expr_element: String,
    stale_is_clean: bool,
    stale_report: String,
    wrong_column_is_clean: bool,
    wrong_column_report: String,
    no_predicate_is_clean: bool,
    no_predicate_report: String,
}

/// One index's rendered predicate, or a marker naming what was missing - an absent
/// index must read as a failed assertion, never as a body that happens not to contain
/// a column name.
fn index_predicate(snapshot: &SchemaSnapshot, index: &str) -> String {
    snapshot
        .tables
        .get(TABLE)
        .and_then(|table| table.indexes.iter().find(|i| i.name == index))
        .map_or_else(
            || format!("<no index {index} on {TABLE}>"),
            |i| {
                i.predicate
                    .clone()
                    .unwrap_or_else(|| format!("<index {index} carries no predicate>"))
            },
        )
}

/// One index's first `Expr` key body, with the same missing-object marker discipline.
fn index_expr_element(snapshot: &SchemaSnapshot, index: &str) -> String {
    snapshot
        .tables
        .get(TABLE)
        .and_then(|table| table.indexes.iter().find(|i| i.name == index))
        .map_or_else(
            || format!("<no index {index} on {TABLE}>"),
            |i| {
                i.elements
                    .iter()
                    .find_map(|element| match element {
                        IndexElementSnapshot::Expr(expr) => Some(expr.clone()),
                        IndexElementSnapshot::Column { .. } => None,
                    })
                    .unwrap_or_else(|| format!("<index {index} has no expression key>"))
            },
        )
}

/// Fold one authored IR document offline, through the same policy resolution the
/// engine applies.
fn fold(
    ir: &str,
    policy: &EffectivePolicy,
    project_schema: &str,
) -> Result<SchemaSnapshot, String> {
    let authored: MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    fold_ops(&resolved.ops, SqlDialect::Postgres, project_schema, policy)
        .map_err(|error| format!("fold the PostgreSQL ops: {error}"))
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
            "fold-stale-index-body-pg",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("{error}"))
}

/// Create the table and both indexes through the real engine, rename the referenced
/// column with PostgreSQL's own DDL, then read BOTH projections of each rendered body
/// and run the differ over the stale pair and the two witnesses. `None` when no live
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
        .expect("create isolated stale-index-body schema");

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
            CREATE_IR,
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

        let folded = fold(FOLDED_IR, &policy, &cfg.project_schema)?;
        let wrong_column = fold(WRONG_COLUMN_IR, &policy, &cfg.project_schema)?;
        let no_predicate = fold(NO_PREDICATE_IR, &policy, &cfg.project_schema)?;
        let live = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;

        let stale = diff_snapshots(&folded, &live);
        let wrong_column_drift = diff_snapshots(&wrong_column, &live);
        let no_predicate_drift = diff_snapshots(&no_predicate, &live);

        Ok(Measured {
            fold_predicate: index_predicate(&folded, PARTIAL_INDEX),
            live_predicate: index_predicate(&live, PARTIAL_INDEX),
            fold_expr_element: index_expr_element(&folded, EXPR_INDEX),
            live_expr_element: index_expr_element(&live, EXPR_INDEX),
            stale_is_clean: stale.is_clean(),
            stale_report: format!("{stale:#?}"),
            wrong_column_is_clean: wrong_column_drift.is_clean(),
            wrong_column_report: format!("{wrong_column_drift:#?}"),
            no_predicate_is_clean: no_predicate_drift.is_clean(),
            no_predicate_report: format!("{no_predicate_drift:#?}"),
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
async fn a_rename_moves_both_folded_index_bodies_and_their_spelling_stays_unreported() {
    let Some(measured) = measure().await else {
        return;
    };
    let Measured {
        fold_predicate,
        live_predicate,
        fold_expr_element,
        live_expr_element,
        stale_is_clean,
        stale_report,
        wrong_column_is_clean,
        wrong_column_report,
        no_predicate_is_clean,
        no_predicate_report,
    } = measured;

    // Both rendered bodies FOLLOW the rename. They are the second and third carriers of
    // the rename carrier sweep, rewritten through the quoted-run walk: a column LIST
    // cannot repair an arbitrary expression, but a walk that copies string literals
    // through whole can move the reference without touching a literal beside it. All
    // three dialect emitters splice these two fields into `CREATE INDEX` verbatim, so a
    // stale body is not a comparison artifact - it is DDL naming a dead column.
    assert!(
        fold_predicate.contains(NEW_COLUMN) && !fold_predicate.contains(OLD_COLUMN),
        "the fold's partial-index predicate must follow \
         `rename {OLD_COLUMN} -> {NEW_COLUMN}`, naming {NEW_COLUMN} and not {OLD_COLUMN}; it \
         read `{fold_predicate}`. Every dialect emitter renders this text straight into \
         `CREATE INDEX ... WHERE <predicate>`, so a stale one is SQL over a column the \
         table no longer has"
    );
    assert!(
        fold_expr_element.contains(NEW_COLUMN) && !fold_expr_element.contains(OLD_COLUMN),
        "the fold's expression KEY must follow the rename for the same reason, naming \
         {NEW_COLUMN} and not {OLD_COLUMN}; it read `{fold_expr_element}`"
    );

    // The witness that makes the quiet-differ assertion mean anything: live genuinely
    // disagrees. `pg_index.indpred` / `indexprs` are parse trees over attribute
    // NUMBERS, so `pg_get_indexdef` deparses the NEW name the instant the rename
    // commits.
    assert!(
        live_predicate.contains(NEW_COLUMN) && !live_predicate.contains(OLD_COLUMN),
        "PostgreSQL deparses a partial predicate over the renamed attribute under its NEW \
         name, so the live predicate must name {NEW_COLUMN} and not {OLD_COLUMN}; it read \
         `{live_predicate}`. With both sides spelling the same column the drift assertion \
         below would prove nothing"
    );
    assert!(
        live_expr_element.contains(NEW_COLUMN) && !live_expr_element.contains(OLD_COLUMN),
        "the live expression KEY must name {NEW_COLUMN} and not {OLD_COLUMN}; it read \
         `{live_expr_element}`"
    );
    assert_ne!(
        fold_predicate, live_predicate,
        "the fold and live predicates must actually diverge for this test to have a subject; \
         both read `{fold_predicate}`"
    );
    assert_ne!(
        fold_expr_element, live_expr_element,
        "the fold and live expression keys must actually diverge for this test to have a \
         subject; both read `{fold_expr_element}`"
    );

    // The consumer that used to expose the staleness: the differ. It byte-compared a
    // hand-rolled renderer against PostgreSQL's deparser, so the divergence above read
    // as drift on every introspection and no apply could clear it.
    assert!(
        stale_is_clean,
        "the differ exempts index expression BODIES, so two stale folded bodies must report \
         NO drift; comparing them makes every column rename permanent drift on the next \
         introspection: {stale_report}"
    );

    // What keeps the exemption honest. The referenced-column SET is recovered from
    // `pg_depend` on the live side and compared, so an expression reading a DIFFERENT
    // column is still caught - with element count, order, kinds and predicate presence
    // all identical, nothing else can see it.
    assert!(
        !wrong_column_is_clean && wrong_column_report.contains(REFERENCED_COLUMNS_FIELD),
        "a fold whose predicate and expression key read `id` where live reads {NEW_COLUMN} \
         must still report drift, under the `{REFERENCED_COLUMNS_FIELD}` field; the exemption \
         drops the TEXT comparison, not the referenced-column set recovered from `pg_depend`: \
         {wrong_column_report}"
    );

    // Presence is not part of the exemption: `None` against `Some` is still drift.
    assert!(
        !no_predicate_is_clean && no_predicate_report.contains("\"predicate\""),
        "a fold carrying NO predicate where live carries one must still report drift, under \
         the `predicate` field; the exemption covers two PRESENT bodies only: \
         {no_predicate_report}"
    );
}
