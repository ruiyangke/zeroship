//! **A MySQL declarative rename is refused at PLAN time, and nothing reaches the
//! server. Asked of a live MySQL server.**
//!
//! # The defect this pins closed
//!
//! `render::declarative`'s rename author used to have no dialect guard. The only gate
//! in the diff span was `if is_sqlite`, which `continue`s past the author; PostgreSQL
//! AND MySQL both reached `ExpandContractAuthor::author` and both pushed an
//! `ExpandContractPlan` into the plan's `renames`. `engine.rs`'s shape-adapter then
//! mapped EVERY `renames` entry to `RenameStep::PgExpandContract` unconditionally, so
//! a MySQL deploy carrying a rename hint reached apply holding a step whose own doc
//! comment labels it **Postgres**, and the MySQL backend's `online()` is `None`.
//!
//! Measured against live MySQL 8 / InnoDB before the fix, the deploy failed
//! mid-apply with `ApplyError::Backend("plan carries a PG online rename but the
//! backend has no online schema-change capability ... a PgExpandContract here is a
//! routing bug")` - an internal-invariant string reaching an operator - and
//! `information_schema.COLUMNS` reported `["id", "email", "nickname"]`. The plain
//! `ADD COLUMN` riding in the same deploy had COMMITTED while the rename had not
//! happened: a schema that was neither the old shape nor the new one, and one no
//! retry could complete, because the refusal was structural and every re-plan landed
//! on the same arm. Three source comments asserted this could not happen
//! (`engine.rs`'s shape-adapter, `mysql/mod.rs`'s `online()`,
//! `apply/backend/mod.rs`'s `blocking_column_dependents`); all three were false, and
//! all three now say what is true.
//!
//! # Why refusing was the fix, and not a promise being withdrawn
//!
//! Everything else in the repo already said MySQL cannot rename a column.
//! `docs/dialects.md`'s "Rename column" row reads `MySQL 8 | No`.
//! `model/dialect_table.rs` records `renameColumn | base | mysql: Unsupported`. The
//! IR lane onto the SAME `ExpandContractAuthor` answers `SqlDialect::Mysql =>
//! Err(UnsupportedInV1)` at plan time. The declarative differ was the lone dissenter
//! and the only path that reached a live server, so the guard makes the code honor a
//! promise it was breaking rather than changing what the product offers.
//!
//! The precedent sits 60 lines below the rename loop in the same function: the
//! `MysqlAlterColumnUnsupported` arm, whose comment already argued exactly this -
//! refuse before rendering so "an authored change and a declarative one refuse alike
//! rather than one lane silently planning invalid DDL". That reasoning had been
//! applied to ALTER COLUMN and not to the rename above it.
//!
//! # What this file asserts, and why it is not an error-type check
//!
//! The load-bearing assertion is that after the refusal `information_schema.COLUMNS`
//! is EXACTLY `["id", "email"]` - the plain `ADD COLUMN` from the same deploy did not
//! commit, no shadow column exists, no dual-write trigger exists, and the seeded row
//! is intact. A plan-time refusal means zero statements executed, and that is the
//! property that protects a user; an assertion on the error's type would pass just as
//! happily on a fix that merely moved the failure one step earlier.
//!
//! So the test drives whichever path the engine takes: it does not stop at
//! `expect_err` on the plan. Delete the guard arm from `render/declarative.rs` and
//! control flows through the `Ok` branch into a real apply, `nickname` commits ahead
//! of the unroutable rename, and the exact-column-list assertion is what goes red.
//!
//! A final leg proves the refusal is SCOPED: the same desired schema deployed with NO
//! rename hint still plans and applies on MySQL, so a guard that broke ordinary MySQL
//! declarative deploys could not pass this file.
//!
//! # The oracle is the server
//!
//! Every assertion reads `information_schema` or `SHOW CREATE TABLE` - never the SQL
//! the engine emitted. Asserting on emitted bytes would only prove the renderer
//! agrees with itself.
//!
//! # The oracle is the server
//!
//! Every assertion below reads `information_schema` or `SHOW CREATE TABLE` - never
//! the SQL the engine emitted. Asserting on emitted bytes would only prove the
//! renderer agrees with itself. The questions asked of MySQL are: which columns does
//! the table have, how many rows, what is in them, and did any shadow column or
//! dual-write trigger from an interrupted EXPAND survive.
//!
//! # Why the deploy carries an additive change beside the rename
//!
//! The shape-adapter orders steps plain DDL -> rebuilds -> renames, and every lowered
//! unit commits in its own transaction. A rename-only deploy therefore cannot
//! distinguish "refused before anything ran" from "refused after everything ran" -
//! both leave the table untouched. The extra plain `ADD COLUMN` is what makes the
//! difference observable, and it is the column whose absence the exact-list assertion
//! turns into a failure.
//!
//! # The PostgreSQL control
//!
//! `postgres_control_*` runs the SAME authored rename through the SAME two public
//! calls against live PostgreSQL. Without it a red MySQL leg cannot be told apart
//! from a broken harness, and a green one cannot be told apart from a test that never
//! reached the path.
//!
//! Gated on `ZERO_MIGRATE_MYSQL_URL` / `ZERO_MIGRATE_TEST_PG_URL`; read the SKIP
//! banner before reading the pass count.

use crate::support;

use std::collections::HashMap;

use crate::support::mysql::{quote_ident as mysql_ident, DatabaseGuard, MysqlDevSession};
use crate::support::PgDevSession;
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    desired_snapshot_for_dialect, snapshot_schema, Approval, CollectionDescriptor,
    DeclarativeAuthor, EffectivePolicy, ExecutorConfig, FieldDescriptor, GuardConfig,
    IndexDescriptor, MigrationEngine, PostgresBackend, RenameHint, SqlDialect,
};

const OWNER: &str = "app_declarative_rename";
const TABLE: &str = "people";
const OLD_COLUMN: &str = "email";
const NEW_COLUMN: &str = "email_address";
/// The plain additive column that rides along in the same deploy as the rename, so a
/// half-applied deploy is observable. See the module doc.
const ADDED_COLUMN: &str = "nickname";
const SEEDED_VALUE: &str = "a@example.com";

fn policy_for(schema: &str) -> EffectivePolicy {
    support::no_inject(schema)
}

/// The `people` collection, spelling its email field `email_column` and optionally
/// carrying the extra plain column.
fn people(email_column: &str, with_added: bool) -> CollectionDescriptor {
    let mut fields = vec![
        FieldDescriptor {
            name: "id".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        },
        FieldDescriptor {
            name: email_column.into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        },
    ];
    if with_added {
        fields.push(FieldDescriptor {
            name: ADDED_COLUMN.into(),
            ty: "string".into(),
            required: false,
            ..Default::default()
        });
    }
    CollectionDescriptor {
        name: TABLE.into(),
        owner_app: OWNER.into(),
        fields,
        indexes: vec![IndexDescriptor {
            name: "people_id_key".into(),
            columns: vec!["id".into()],
            unique: true,
        }],
        runtime_options: Default::default(),
    }
}

fn hints() -> Vec<RenameHint> {
    vec![RenameHint {
        table: TABLE.into(),
        from: OLD_COLUMN.into(),
        to: NEW_COLUMN.into(),
    }]
}

// ---------------------------------------------------------------------------
// The MySQL leg.
// ---------------------------------------------------------------------------

/// The columns MySQL itself reports for `table` in `database`, in ordinal order.
async fn mysql_columns(session: &MysqlDevSession, database: &str, table: &str) -> Vec<String> {
    session
        .query(
            &format!(
                "SELECT COLUMN_NAME AS c FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = '{database}' AND TABLE_NAME = '{table}' \
                 ORDER BY ORDINAL_POSITION"
            ),
            &[],
        )
        .await
        .expect("read information_schema.COLUMNS")
        .into_iter()
        .map(|row| row.try_get::<_, String>("c").expect("decode COLUMN_NAME"))
        .collect()
}

/// Every trigger MySQL reports in `database` - the EXPAND phase's dual-write trigger
/// would show up here if any part of it had landed.
async fn mysql_triggers(session: &MysqlDevSession, database: &str) -> Vec<String> {
    session
        .query(
            &format!(
                "SELECT TRIGGER_NAME AS t FROM information_schema.TRIGGERS \
                 WHERE TRIGGER_SCHEMA = '{database}' ORDER BY TRIGGER_NAME"
            ),
            &[],
        )
        .await
        .expect("read information_schema.TRIGGERS")
        .into_iter()
        .map(|row| row.try_get::<_, String>("t").expect("decode TRIGGER_NAME"))
        .collect()
}

async fn mysql_show_create(session: &MysqlDevSession, database: &str, table: &str) -> String {
    let row = session
        .query_one(
            &format!(
                "SHOW CREATE TABLE {}.{}",
                mysql_ident(database),
                mysql_ident(table)
            ),
            &[],
        )
        .await
        .expect("SHOW CREATE TABLE");
    row.try_get::<_, String>("Create Table")
        .expect("decode SHOW CREATE TABLE")
}

async fn mysql_row_count(session: &MysqlDevSession, database: &str, table: &str) -> i64 {
    let row = session
        .query_one(
            &format!(
                "SELECT COUNT(*) AS n FROM {}.{}",
                mysql_ident(database),
                mysql_ident(table)
            ),
            &[],
        )
        .await
        .expect("count rows");
    row.try_get::<_, i64>("n").expect("decode COUNT(*)")
}

#[compio::test]
async fn a_mysql_declarative_rename_is_refused_at_plan_time_and_nothing_reaches_the_server() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("decl_rename");
    let cfg = ExecutorConfig::new(
        format!("project_{database}"),
        database.clone(),
        policy_for(&database),
    );
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", mysql_ident(&database)))
        .await
        .expect("create the isolated declarative-rename database");

    let engine = MigrationEngine::new();
    let author = DeclarativeAuthor::new_for_dialect(database.clone(), OWNER, SqlDialect::Mysql);
    let guard = GuardConfig::from_policy(policy_for(&database), SqlDialect::Mysql);
    let backend = MysqlBackend::new_generic(&session);

    // v1: create the table, then WRITE A ROW. Without data a rename cannot lose
    // anything, so the test would pass on a lossy implementation.
    let v1 = people(OLD_COLUMN, false);
    let desired1 = desired_snapshot_for_dialect(
        &database,
        std::slice::from_ref(&v1),
        SqlDialect::Mysql,
        &policy_for(&database),
    )
    .expect("desired v1");
    let live_empty = backend
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot live MySQL (empty)");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard,
            &policy_for(&database),
        )
        .expect("plan v1");
    engine
        .apply_declarative(
            &plan1,
            &policy_for(&database),
            Approval::Approved,
            &backend,
            &cfg,
            "declarative-rename-mysql",
        )
        .await
        .expect("apply v1");

    session
        .exec(
            &format!(
                "INSERT INTO {}.{} (`id`, `{OLD_COLUMN}`) VALUES ('p1', '{SEEDED_VALUE}')",
                mysql_ident(&database),
                mysql_ident(TABLE)
            ),
            &[],
        )
        .await
        .expect("seed a row before the rename");

    // v2: the same collection with the field renamed AND one plain column added, so
    // a half-applied deploy is observable.
    let v2 = people(NEW_COLUMN, true);
    let desired2 = desired_snapshot_for_dialect(
        &database,
        std::slice::from_ref(&v2),
        SqlDialect::Mysql,
        &policy_for(&database),
    )
    .expect("desired v2");
    let live_v1 = backend
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot live MySQL (v1)");
    let planned = engine.plan_declarative(
        &desired2,
        &live_v1,
        &HashMap::new(),
        &author,
        &hints(),
        &guard,
        &policy_for(&database),
    );

    // **Drive whichever path the engine actually takes.** This deliberately does NOT
    // stop at `expect_err` on the plan: if the plan-time refusal is ever lost, the
    // planner succeeds and the deploy must then be APPLIED, because the property
    // being measured is "nothing reached the server", not "an error had the right
    // type". Removing the guard arm from `render/declarative.rs` sends control
    // through the `Ok` branch below, the plain `ADD COLUMN` commits ahead of the
    // unroutable rename, and the `information_schema` assertions after this block are
    // what fail. That is the red this test is built to produce.
    let refusal = match planned {
        Err(error) => error.to_string(),
        Ok(plan) => engine
            .apply_declarative(
                &plan,
                &policy_for(&database),
                Approval::Approved,
                &backend,
                &cfg,
                "declarative-rename-mysql",
            )
            .await
            .expect_err("a MySQL expand-contract rename cannot apply either")
            .to_string(),
    };

    // THE ORACLE: what MySQL itself says the table looks like after the refusal.
    //
    // Asked FIRST, before anything about the error text. The property is what
    // protects a user and the message is a courtesy, so the property is what should
    // name the regression when this file goes red - a fix that merely moved the
    // failure one step earlier would still produce a plausible-looking error.
    let columns = mysql_columns(&session, &database, TABLE).await;
    let triggers = mysql_triggers(&session, &database).await;
    let create = mysql_show_create(&session, &database, TABLE).await;
    let rows = mysql_row_count(&session, &database, TABLE).await;

    // **NOTHING IS HALF-APPLIED**, and this is the assertion that protects the user.
    // A plan-time refusal means zero statements executed, so the plain `ADD COLUMN`
    // that rode along in the same deploy must NOT be here. Before the fix it was: the
    // shape-adapter orders plain DDL ahead of renames and each lowered unit commits
    // in its own transaction, so `nickname` committed and stayed committed while the
    // rename died, leaving a schema that was neither shape and that no retry could
    // complete. Stated as an EXACT column list rather than three `!contains` checks,
    // so an unexpected column is a failure too.
    assert_eq!(
        columns,
        vec!["id".to_string(), OLD_COLUMN.to_string()],
        "a refused deploy must leave the table EXACTLY as it was - no half-applied \
         plain DDL, no shadow column: {columns:?}\n{create}"
    );
    assert!(
        triggers.is_empty(),
        "no dual-write trigger may exist: {triggers:?}"
    );
    assert_eq!(rows, 1, "the seeded row is untouched");
    let row = session
        .query_one(
            &format!(
                "SELECT `{OLD_COLUMN}` AS v FROM {}.{}",
                mysql_ident(&database),
                mysql_ident(TABLE)
            ),
            &[],
        )
        .await
        .expect("read the seeded row back");
    assert_eq!(
        row.try_get::<_, String>("v").expect("decode"),
        SEEDED_VALUE,
        "the seeded value is intact under its original name"
    );

    // Only now the message. The refusal is the differ's own typed one, raised before
    // a single statement was rendered - NOT the engine's `ApplyError::Backend("... a
    // PgExpandContract here is a routing bug")`, which is an internal-invariant
    // message and was what an operator used to see.
    assert!(
        refusal.contains("cannot rename column"),
        "the refusal must be the differ's typed plan-time one: {refusal}"
    );
    assert!(
        refusal.contains("MySQL"),
        "the refusal names the dialect it stopped for: {refusal}"
    );
    assert!(
        !refusal.contains("routing bug"),
        "an internal routing-bug message must never reach an operator: {refusal}"
    );

    // **The refusal is SCOPED to the rename.** A guard that stopped ordinary MySQL
    // declarative deploys would pass every assertion above while breaking the
    // dialect, so the same desired schema is deployed again with NO rename hint. With
    // no hint the differ cannot infer a rename, so `email` -> `email_address` becomes
    // a plain drop-plus-add, which MySQL renders and applies natively. This must
    // still work.
    let live_after_refusal = backend
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot live MySQL (unchanged)");
    let unhinted = engine
        .plan_declarative(
            &desired2,
            &live_after_refusal,
            &HashMap::new(),
            &author,
            &[],
            &guard,
            &policy_for(&database),
        )
        .expect("an unhinted MySQL declarative deploy must still plan");
    assert!(
        unhinted.renames.is_empty(),
        "without a hint there is no rename to refuse: {:?}",
        unhinted.renames
    );
    engine
        .apply_declarative(
            &unhinted,
            &policy_for(&database),
            Approval::Approved,
            &backend,
            &cfg,
            "declarative-rename-mysql",
        )
        .await
        .expect("an unhinted MySQL declarative deploy must still apply");
    let after = mysql_columns(&session, &database, TABLE).await;
    assert!(
        after.iter().any(|c| c == NEW_COLUMN) && after.iter().any(|c| c == ADDED_COLUMN),
        "the unhinted deploy reshapes the table normally: {after:?}"
    );
    assert!(
        !after.iter().any(|c| c == OLD_COLUMN),
        "and it drops the old column, which is exactly why a rename wants a hint: \
         {after:?}"
    );
}

// ---------------------------------------------------------------------------
// The PostgreSQL control.
// ---------------------------------------------------------------------------

fn pg_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "decl_rename_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn pg_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn pg_columns(session: &PgDevSession, schema: &str, table: &str) -> Vec<String> {
    session
        .query(
            "SELECT column_name AS c FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
            &[schema.into(), table.into()],
        )
        .await
        .expect("read information_schema.columns")
        .into_iter()
        .map(|row| row.try_get::<_, String>("c").expect("decode column_name"))
        .collect()
}

#[compio::test]
async fn postgres_control_the_same_declarative_rename_applies_and_the_rows_survive() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = pg_token();
    let mut cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        schema.clone(),
        policy_for(&schema),
    );
    cfg.confinement.meta_schema = format!("{schema}_meta");
    let _schemas = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {}", pg_ident(&schema)))
        .await
        .expect("create the isolated declarative-rename schema");

    let engine = MigrationEngine::new();
    let author = DeclarativeAuthor::new_for_dialect(schema.clone(), OWNER, SqlDialect::Postgres);
    let guard = GuardConfig::from_policy(policy_for(&schema), SqlDialect::Postgres);
    let backend = PostgresBackend::new_generic(&session);

    let v1 = people(OLD_COLUMN, false);
    let desired1 = desired_snapshot_for_dialect(
        &schema,
        std::slice::from_ref(&v1),
        SqlDialect::Postgres,
        &policy_for(&schema),
    )
    .expect("desired v1");
    let live_empty = snapshot_schema(&session, &schema)
        .await
        .expect("snapshot live PG (empty)");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &live_empty,
            &HashMap::new(),
            &author,
            &[],
            &guard,
            &policy_for(&schema),
        )
        .expect("plan v1");
    engine
        .apply_declarative(
            &plan1,
            &policy_for(&schema),
            Approval::Approved,
            &backend,
            &cfg,
            "declarative-rename-pg",
        )
        .await
        .expect("apply v1");

    session
        .exec(
            &format!(
                "INSERT INTO {}.{} (\"id\", \"{OLD_COLUMN}\") VALUES ('p1', '{SEEDED_VALUE}')",
                pg_ident(&schema),
                pg_ident(TABLE)
            ),
            &[],
        )
        .await
        .expect("seed a row before the rename");

    let v2 = people(NEW_COLUMN, true);
    let desired2 = desired_snapshot_for_dialect(
        &schema,
        std::slice::from_ref(&v2),
        SqlDialect::Postgres,
        &policy_for(&schema),
    )
    .expect("desired v2");
    let live_v1 = snapshot_schema(&session, &schema)
        .await
        .expect("snapshot live PG (v1)");
    let plan2 = engine
        .plan_declarative(
            &desired2,
            &live_v1,
            &HashMap::new(),
            &author,
            &hints(),
            &guard,
            &policy_for(&schema),
        )
        .expect("plan v2 with a hint");
    assert_eq!(plan2.renames.len(), 1, "the PG plan carries a rename");
    assert_eq!(
        plan2.plain.items.len(),
        1,
        "the plain set carries exactly the ADD COLUMN"
    );

    engine
        .apply_declarative(
            &plan2,
            &policy_for(&schema),
            Approval::Approved,
            &backend,
            &cfg,
            "declarative-rename-pg",
        )
        .await
        .expect("apply the online rename on PostgreSQL");

    let columns = pg_columns(&session, &schema, TABLE).await;
    assert!(
        columns.iter().any(|c| c == NEW_COLUMN),
        "the renamed column must exist: {columns:?}"
    );

    let row = session
        .query_one(
            &format!(
                "SELECT \"{NEW_COLUMN}\" AS v FROM {}.{}",
                pg_ident(&schema),
                pg_ident(TABLE)
            ),
            &[],
        )
        .await
        .expect("the renamed column must be queryable");
    let got: String = row.try_get::<_, String>("v").expect("decode");
    assert_eq!(
        got, SEEDED_VALUE,
        "the row written before the rename must survive it"
    );
}
