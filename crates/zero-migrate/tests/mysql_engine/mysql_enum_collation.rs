//! A MySQL `ENUM` column is a CHARACTER column, so it must pin the same explicit
//! collation every other character column pins.
//!
//! # The contract, and the hole in it
//!
//! `render::declarative::mysql_type_override_with_collation` states the engine's
//! promise: *"every character type pins an explicit collation so string comparison is
//! case-SENSITIVE by default (matching Postgres/SQLite) unless `caseSensitive: false`
//! asks for a case-insensitive collation"*. `VARCHAR`, `CHAR` and the `TEXT` family
//! honour it. `ENUM` did not, and MySQL treats `ENUM` as a character type whose member
//! lookup, comparison and uniqueness all run under the column's collation.
//!
//! An uncollated `ENUM` therefore inherits the table default, which on a stock MySQL 8
//! server is `utf8mb4_0900_ai_ci` - accent- and case-INSENSITIVE. MEASURED against
//! MySQL 8.4.11 with `@@collation_server = utf8mb4_0900_ai_ci`, the same authored
//! schema then behaves three ways that the other two dialects do not:
//!
//! 1. `INSERT ... VALUES ('ACTIVE')` into `ENUM('active','archived')` SUCCEEDS and
//!    silently stores `'active'`. PostgreSQL raises `22P02 invalid input value for
//!    enum ... "ACTIVE"`. A value the author never declared becomes a value the author
//!    did declare, without an error, on one backend only.
//! 2. An enum whose members differ only in case - `['active', 'Active']` - CREATES on
//!    PostgreSQL and SQLite and is REFUSED outright by MySQL:
//!    `ERROR 1291 Column 'status' has duplicated value 'active' in ENUM`. The same
//!    authored schema cannot be deployed at all.
//! 3. Structural drift never converges. The server reports `utf8mb4_0900_ai_ci`, which
//!    `case_sensitive_from_collation` normalizes to `Some(false)`, while the desired
//!    snapshot says nothing. So a table the engine created and nobody touched reports
//!    `column status / case_sensitive / expected "" actual "false"` on every run,
//!    forever.
//!
//! `fold_roundtrip_mysql.rs` stage (9) records all three and deliberately omits an
//! enum column because of them.
//!
//! # What these tests measure, and what makes them honest
//!
//! The subject is BEHAVIOUR on a live server, not DDL spelling. Grepping the emitted
//! statement for `COLLATE utf8mb4_0900_as_cs` would prove only that the renderer
//! agrees with itself; it is the server that decides whether `'ACTIVE'` is a new value
//! or an old one. So every test here asks MySQL, and the case-sensitivity claims ask
//! PostgreSQL the same question so that the DISAGREEMENT is what fails.
//!
//! [`the_probe_server_is_case_insensitive_by_default`] is the instrument check and it
//! is load-bearing: on a server whose default collation is already `_cs` or `_bin`, a
//! pinned and an unpinned enum behave identically and every assertion below passes
//! while proving nothing. It fails rather than skips, so a mis-provisioned server
//! cannot read as coverage.

use crate::support;

use std::collections::{BTreeMap, HashMap};

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::MysqlBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::snapshot::SchemaSnapshot;
use zero_migrate::render::declarative::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor,
};
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_create_table_policy, Approval, ExecutorConfig, GuardConfig,
    IrAuthor, LiveSchema, LockMode, MigrationBackend, MigrationEngine, MigrationIr, SqlDialect,
};

const OWNER: &str = "app_enum_collation";

/// A `createEnum` plus a table using it, over `values`, with a plain-text control
/// column so a collation claim about the enum is distinguishable from one about the
/// whole table.
fn source(values: &str) -> String {
    format!(
        r#"{{"ir_version":1,"name":"create_issues","owner_app":"{OWNER}","ops":[
        {{"op":"createEnum","name":"issue_status","values":{values}}},
        {{"op":"createTable","name":"issues","columns":[
            {{"name":"id","type":"int","nullable":false}},
            {{"name":"status","type":{{"enum":{{"name":"issue_status"}}}},"nullable":false}},
            {{"name":"label","type":"text","nullable":true}}
        ],"primaryKey":["id"]}}
    ]}}"#
    )
}

fn cfg_for(database: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{database}"),
        database,
        support::no_inject(database),
    )
}

/// Lower one IR doc for `dialect` and return the statements, so a test can show the
/// server the SAME DDL the engine would deploy.
fn lower(dialect: SqlDialect, schema: &str, values: &str) -> Result<Vec<String>, String> {
    let policy = support::no_inject(schema);
    let authored: MigrationIr =
        serde_json::from_str(&source(values)).map_err(|e| format!("parse the test IR: {e}"))?;
    let author = IrAuthor::new(schema, OWNER, dialect, &policy);
    Ok(author
        .lower(&authored, &LiveSchema::default())
        .map_err(|e| format!("lower the test IR: {e}"))?
        .into_iter()
        .map(|m| m.up)
        .collect())
}

/// Deploy one IR doc through the REAL MySQL pipeline and return the resolved ops, so
/// the caller can fold the SAME stream the engine replayed.
async fn deploy_mysql(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    values: &str,
) -> Result<Vec<zero_migrate::model::ir::Op>, String> {
    let policy = support::no_inject(&cfg.project_schema);
    let authored: MigrationIr =
        serde_json::from_str(&source(values)).map_err(|e| format!("parse the test IR: {e}"))?;
    let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
        .map_err(|e| format!("resolve create-table policy: {e}"))?;
    let resolved_source =
        serde_json::to_string(&resolved).map_err(|e| format!("serialize resolved IR: {e}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Mysql, &policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Mysql);
    let registry: BTreeMap<String, String> = BTreeMap::new();
    let artifact = author
        .load_and_lower_guarded(
            &resolved_source,
            OWNER,
            &registry,
            &LiveSchema::default(),
            &guard,
        )
        .map_err(|e| format!("load and lower the guarded plan: {e}"))?;
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &MysqlBackend::new_generic(session),
            cfg,
            "mysql-enum-collation",
            LockMode::Acquire,
        )
        .await
        .map_err(|e| format!("apply the plan: {e}"))?;
    Ok(resolved.ops)
}

/// `CREATE DATABASE` for one probe, and the guard that drops it.
async fn fresh_database(session: &MysqlDevSession, database: &str) -> Result<(), String> {
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(database)))
        .await
        .map_err(|e| format!("create the probe database: {e}"))
}

/// The collation MySQL would give an uncollated character column in `database`.
async fn database_collation(session: &MysqlDevSession, database: &str) -> Result<String, String> {
    let row = session
        .query_one(
            "SELECT DEFAULT_COLLATION_NAME AS c FROM information_schema.schemata \
             WHERE SCHEMA_NAME = ?",
            &[zero_migrate::driver::Bind::Text(database.to_string())],
        )
        .await
        .map_err(|e| format!("read the database default collation: {e}"))?;
    row.try_get::<_, String>("c")
        .map_err(|e| format!("DEFAULT_COLLATION_NAME did not decode as text: {e}"))
}

/// The collation the SERVER gave `column`, read from the catalog.
///
/// `information_schema.COLUMNS` is the relation the SHIPPED drift path reads
/// (`apply::backend::mysql::drift_sql`), so reading it here measures the same surface
/// the engine measures. It is privilege-filtered - MySQL shows a row only for a column
/// the connected user holds some privilege on - so an "absent" row would be
/// indistinguishable from an invisible one. The caller therefore treats a missing row
/// as an ERROR rather than as "no collation", and
/// [`show_create_table`] corroborates from a second relation whose text says whether
/// the collation was PINNED by the engine or INHERITED from the table.
async fn catalog_collation(
    session: &MysqlDevSession,
    database: &str,
    table: &str,
    column: &str,
) -> Result<Option<String>, String> {
    let rows = session
        .query(
            "SELECT COLLATION_NAME AS c FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?",
            &[
                zero_migrate::driver::Bind::Text(database.to_string()),
                zero_migrate::driver::Bind::Text(table.to_string()),
                zero_migrate::driver::Bind::Text(column.to_string()),
            ],
        )
        .await
        .map_err(|e| format!("read the column collation: {e}"))?;
    match rows.len() {
        1 => Ok(rows[0].try_get::<_, String>("c").ok()),
        // Never "the column has no collation": that is what an unprivileged read looks
        // like too, and the two must not be conflated.
        n => Err(format!(
            "information_schema.COLUMNS returned {n} rows for {database}.{table}.{column}; \
             an absent row and a row this user cannot see are the same thing here"
        )),
    }
}

/// `SHOW CREATE TABLE`, the corroborating witness: its text carries the column's
/// EXPLICIT collation clause, so it distinguishes a collation the engine PINNED from
/// one the column merely INHERITED from the table default.
async fn show_create_table(
    session: &MysqlDevSession,
    database: &str,
    table: &str,
) -> Result<String, String> {
    let row = session
        .query_one(
            &format!(
                "SHOW CREATE TABLE {}.{}",
                quote_ident(database),
                quote_ident(table)
            ),
            &[],
        )
        .await
        .map_err(|e| format!("SHOW CREATE TABLE: {e}"))?;
    row.try_get::<_, String>("Create Table")
        .map_err(|e| format!("SHOW CREATE TABLE did not decode as text: {e}"))
}

/// THE INSTRUMENT CHECK. Every claim in this file is void on a server that is already
/// case-sensitive by default, because then a pinned and an unpinned enum agree.
///
/// It FAILS rather than skips: a green suite on a mis-provisioned server would be the
/// exact false reassurance this file exists to prevent.
#[compio::test]
async fn the_probe_server_is_case_insensitive_by_default() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("enumcollinstr");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let collation = database_collation(&session, &database).await?;
        if !collation.contains("_ci") {
            return Err(format!(
                "a fresh database on this server defaults to {collation}, which is not \
                 case-INSENSITIVE, so nothing in this file distinguishes a pinned enum \
                 collation from an absent one (server {})",
                session.server_version()
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The behavioural half, and the cross-dialect disagreement that is the finding.
///
/// PostgreSQL refuses `'ACTIVE'` for an enum declaring `'active'`. MySQL must refuse
/// it too. Before the fix MySQL ACCEPTED it and stored `'active'`, so a value the
/// author never declared silently became one that was.
#[compio::test]
async fn a_wrong_case_enum_member_is_refused_on_mysql_as_it_is_on_postgres() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("enumcase");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let collation = database_collation(&session, &database).await?;
        if !collation.contains("_ci") {
            return Err(format!(
                "this database defaults to {collation}; the assertion below cannot \
                 distinguish the fix from its absence"
            ));
        }
        deploy_mysql(&session, &cfg, r#"["active","archived"]"#).await?;

        // The catalog, read from the relation the shipped drift path reads.
        let column = catalog_collation(&session, &database, "issues", "status")
            .await?
            .ok_or_else(|| {
                "the enum column reported no collation at all, which MySQL does not do \
                 for a character column"
                    .to_string()
            })?;
        if column.contains("_ci") {
            return Err(format!(
                "the enum column carries {column}, a case-INSENSITIVE collation, on a \
                 {collation} database - the engine pinned nothing and the table default \
                 leaked in"
            ));
        }

        // The corroborating witness: an EXPLICIT clause in the table text is what
        // separates "the engine pinned this" from "the table default happened to
        // agree".
        let create = show_create_table(&session, &database, "issues").await?;
        if !create
            .to_ascii_lowercase()
            .contains("collate utf8mb4_0900_as_cs")
        {
            return Err(format!(
                "no explicit collation on the enum column in the table text, so the \
                 catalog reading above is inherited rather than pinned:\n{create}"
            ));
        }

        // THE BEHAVIOURAL ASSERTION. More convincing than any catalog string: it is
        // the server deciding whether 'ACTIVE' is a declared value.
        let insert = session
            .batch(&format!(
                "INSERT INTO {}.`issues` (id, status) VALUES (1, 'ACTIVE')",
                quote_ident(&database)
            ))
            .await;
        if insert.is_ok() {
            let rows = session
                .query(
                    &format!("SELECT status FROM {}.`issues`", quote_ident(&database)),
                    &[],
                )
                .await
                .map_err(|e| format!("read back the coerced value: {e}"))?;
            return Err(format!(
                "MySQL ACCEPTED 'ACTIVE' into ENUM('active','archived') and stored \
                 {rows:?}; PostgreSQL raises 22P02 for the same authored schema"
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The PostgreSQL half of the same question, so the claim above is a DISAGREEMENT
/// between two servers rather than an opinion about one.
#[compio::test]
async fn a_wrong_case_enum_member_is_refused_on_postgres() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("zm_enumcase_{}", std::process::id());
    let _guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    let result: Result<(), String> = async {
        session
            .batch(&format!("CREATE SCHEMA \"{schema}\""))
            .await
            .map_err(|e| format!("create the probe schema: {e}"))?;
        for statement in lower(SqlDialect::Postgres, &schema, r#"["active","archived"]"#)? {
            session
                .batch(&statement)
                .await
                .map_err(|e| format!("apply {statement}: {e}"))?;
        }
        let insert = session
            .batch(&format!(
                "INSERT INTO \"{schema}\".\"issues\" (id, status) VALUES (1, 'ACTIVE')"
            ))
            .await;
        if insert.is_ok() {
            return Err(
                "PostgreSQL accepted 'ACTIVE' for an enum declaring 'active', so the \
                 MySQL assertion is not a cross-dialect disagreement"
                    .to_string(),
            );
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The sharpest half: an enum whose members differ only in case CREATES on PostgreSQL
/// and SQLite and, without a pinned case-sensitive collation, cannot be created on
/// MySQL AT ALL - `ERROR 1291 ... has duplicated value 'active' in ENUM`. The same
/// authored schema deploys on two dialects and fails on the third.
#[compio::test]
async fn an_enum_whose_members_differ_only_in_case_deploys_on_mysql() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("enumdup");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let collation = database_collation(&session, &database).await?;
        if !collation.contains("_ci") {
            return Err(format!(
                "this database defaults to {collation}; MySQL would accept the \
                 case-pair enum with or without the fix"
            ));
        }
        // SQLite renders the same members as an inline CHECK and PostgreSQL as a
        // native type; both accept the pair. Lowering them here keeps the claim about
        // the AUTHORED schema rather than about one dialect's SQL.
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            lower(dialect, "app", r#"["active","Active"]"#).map_err(|e| {
                format!("{dialect:?} could not even render the case-pair enum: {e}")
            })?;
        }
        deploy_mysql(&session, &cfg, r#"["active","Active"]"#).await?;

        // Both members survived as DISTINCT values, which is the point of the pair.
        let create = show_create_table(&session, &database, "issues").await?;
        if !create.contains("'active'") || !create.contains("'Active'") {
            return Err(format!(
                "the deployed enum lost one of its two case-distinct members:\n{create}"
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The drift half, which needs no speculation: a table the engine created and nobody
/// touched must diff clean against the fold of its own op stream, and must keep
/// diffing clean.
///
/// Run TWICE deliberately. The defect this replaces was a diff that NEVER converged -
/// `column status / case_sensitive / expected "" actual "false"` on every run - and a
/// single pass cannot tell a one-off from a permanent one.
#[compio::test]
async fn a_deployed_enum_column_does_not_drift_against_its_own_fold() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("enumdrift");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let collation = database_collation(&session, &database).await?;
        if !collation.contains("_ci") {
            return Err(format!(
                "this database defaults to {collation}, so an uncollated enum would \
                 introspect case-SENSITIVE and this test would pass without the fix"
            ));
        }
        let ops = deploy_mysql(&session, &cfg, r#"["active","archived"]"#).await?;
        let expected = fold_ops(
            &ops,
            SqlDialect::Mysql,
            &cfg.project_schema,
            &support::no_inject(&cfg.project_schema),
        )
        .map_err(|e| format!("fold the op stream offline: {e}"))?;

        for pass in 1..=2 {
            let actual = MysqlBackend::new_generic(&session)
                .snapshot_schema(&cfg)
                .await
                .map_err(|e| format!("snapshot the deployed schema on pass {pass}: {e}"))?;
            let drift = diff_snapshots(&expected, &actual);
            if !drift.is_clean() {
                return Err(format!(
                    "pass {pass}: the engine deployed this table and nobody touched it, \
                     yet drift reported {drift:#?}"
                ));
            }
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// WHY THE PIN CAN BE UNCONDITIONAL: an IR enum column cannot carry
/// `caseSensitive: false` at all.
///
/// The obvious risk in pinning `_as_cs` is trading the old phantom drift for a new one
/// pointing the other way - an author asks for case-insensitivity, the snapshot
/// records `Some(false)`, the server reports `_as_cs`, and
/// `case_sensitive_from_collation` normalizes that back to `None`. MEASURED instead of
/// assumed: the load-and-validate gate REFUSES the combination before anything is
/// rendered, so `source.case_sensitive` is always `None` on a named-enum column and
/// the opposite-direction drift has no way to exist.
///
/// This also RETIRES a guess worth recording, because it is the guess that made this
/// look like a smaller defect than it is: `mysql_base_column_type` returns `"text"`
/// whenever `case_sensitive` is `Some(false)`, which reads as "a `caseSensitive:false`
/// enum silently stops being an enum". On the IR path it never gets that far - the
/// gate refuses it first. `mysql_pin_enum_collation` still READS the facet, because
/// the DESCRIPTOR path reaches `ENUM(...)` from a `text` column plus a CHECK, where
/// the gate does allow it.
#[test]
fn an_ir_enum_column_cannot_declare_case_insensitivity() {
    let policy = support::no_inject("app");
    let src = format!(
        r#"{{"ir_version":1,"name":"create_issues","owner_app":"{OWNER}","ops":[
        {{"op":"createEnum","name":"issue_status","values":["active","archived"]}},
        {{"op":"createTable","name":"issues","columns":[
            {{"name":"id","type":"int","nullable":false}},
            {{"name":"status","type":{{"enum":{{"name":"issue_status"}}}},
              "nullable":false,"caseSensitive":false}}
        ],"primaryKey":["id"]}}
    ]}}"#
    );
    let author = IrAuthor::new("app", OWNER, SqlDialect::Mysql, &policy);
    let guard = GuardConfig::from_policy(policy, SqlDialect::Mysql);
    let registry: BTreeMap<String, String> = BTreeMap::new();
    let refusal = author
        .load_and_lower_guarded(&src, OWNER, &registry, &LiveSchema::default(), &guard)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| {
            panic!(
                "a caseSensitive:false enum column was accepted; the unconditional \
                 case-sensitive pin in mysql_pin_enum_collation would then be dropping \
                 a facet the author declared"
            )
        });
    assert!(
        refusal.contains("caseSensitive:false is only valid on a text column"),
        "the refusal must name the facet and the reason, so the pin's precondition is \
         readable from the failure; got: {refusal}"
    );
}

// ---------------------------------------------------------------------------
// The DESCRIPTOR path, which reaches `ENUM(...)` by a completely different route.
// ---------------------------------------------------------------------------
//
// A collection descriptor never carries an `enum(...)` type. It carries a `string`
// field plus `enum_values`, which `field_check_constraints` turns into a
// `CHECK ("col" IN (...))`, which `render_create_table_mysql_snapshot_statements`
// then folds BACK into a native `ENUM(...)` on MySQL - replacing the column's whole
// rendered type, and with it the collation `column_type_for_render` had pinned.
//
// These two tests exist because a NEUTER found the gap: removing the pin from that
// one arm broke nothing in the suite while every other neuter was caught, which meant
// the descriptor route had no coverage at all. That is the difference between a fix
// that is tested and a fix that merely happens to be present.

/// One collection whose `status` field is a closed set, optionally case-insensitive.
fn enum_descriptor(case_sensitive: Option<bool>) -> CollectionDescriptor {
    CollectionDescriptor {
        name: "issues".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![
            FieldDescriptor {
                name: "status".to_string(),
                ty: "string".to_string(),
                required: true,
                enum_values: Some(vec![
                    serde_json::Value::String("active".to_string()),
                    serde_json::Value::String("archived".to_string()),
                ]),
                case_sensitive,
                ..Default::default()
            },
            FieldDescriptor {
                name: "label".to_string(),
                ty: "string".to_string(),
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// The MySQL `CREATE TABLE` the declarative differ plans for `descriptor` on a first
/// deploy (an empty live schema).
fn descriptor_create_ddl(
    project: &str,
    descriptor: &CollectionDescriptor,
) -> Result<String, String> {
    let effective = support::no_inject(project);
    let desired = desired_snapshot_for_dialect(
        project,
        std::slice::from_ref(descriptor),
        SqlDialect::Mysql,
        &effective,
    )
    .map_err(|e| format!("build the desired snapshot: {e}"))?;
    let plan = DeclarativeAuthor::new_for_dialect(project, OWNER, SqlDialect::Mysql)
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &effective,
        )
        .map_err(|e| format!("diff against an empty live schema: {e}"))?;
    plan.migrations
        .iter()
        .map(|m| m.up.clone())
        .find(|up| up.contains("CREATE TABLE"))
        .ok_or_else(|| "the first-deploy plan carried no CREATE TABLE".to_string())
}

/// A descriptor-authored enum must behave on the server exactly like an IR-authored
/// one: `'ACTIVE'` is not `'active'`.
#[compio::test]
async fn a_descriptor_authored_enum_is_case_sensitive_on_the_server() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("enumdesc");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let collation = database_collation(&session, &database).await?;
        if !collation.contains("_ci") {
            return Err(format!(
                "this database defaults to {collation}; the assertion below cannot \
                 distinguish the fix from its absence"
            ));
        }
        let ddl = descriptor_create_ddl(&database, &enum_descriptor(None))?;
        if !ddl.contains("ENUM(") {
            return Err(format!(
                "the descriptor route stopped producing a native MySQL ENUM, so this \
                 test no longer covers the arm it was written for:\n{ddl}"
            ));
        }
        for statement in ddl.split(";\n") {
            session
                .batch(statement)
                .await
                .map_err(|e| format!("apply {statement}: {e}"))?;
        }

        let column = catalog_collation(&session, &database, "issues", "status")
            .await?
            .ok_or_else(|| "the enum column reported no collation".to_string())?;
        if column.contains("_ci") {
            return Err(format!(
                "a descriptor-authored enum landed on {column} on a {collation} \
                 database; the create-table arm dropped the pin:\n{ddl}"
            ));
        }
        // The server, not the DDL text, is the witness.
        let insert = session
            .batch(&format!(
                "INSERT INTO {}.`issues` (status, label) VALUES ('ACTIVE', 'x')",
                quote_ident(&database)
            ))
            .await;
        if insert.is_ok() {
            return Err(
                "MySQL accepted 'ACTIVE' into a descriptor-authored ENUM('active','archived')"
                    .to_string(),
            );
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The descriptor route is also the ONE route where `caseSensitive: false` reaches an
/// enum: the field is a `string`, so the load gate that refuses the facet on an IR
/// enum column does not apply. The pin must therefore READ the facet rather than
/// hard-code case sensitivity - which is why `mysql_pin_enum_collation` takes it as a
/// parameter instead of being a constant suffix.
#[compio::test]
async fn a_descriptor_enum_asking_for_case_insensitivity_gets_it() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("enumdescci");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let ddl = descriptor_create_ddl(&database, &enum_descriptor(Some(false)))?;
        if !ddl.contains("ENUM(") {
            return Err(format!(
                "a caseSensitive:false descriptor enum stopped rendering as a native \
                 ENUM, so the facet claim below is about a different column:\n{ddl}"
            ));
        }
        for statement in ddl.split(";\n") {
            session
                .batch(statement)
                .await
                .map_err(|e| format!("apply {statement}: {e}"))?;
        }
        let column = catalog_collation(&session, &database, "issues", "status")
            .await?
            .ok_or_else(|| "the enum column reported no collation".to_string())?;
        if !column.contains("_ci") {
            return Err(format!(
                "caseSensitive:false was declared and the enum landed on {column}; the \
                 facet the author DID declare was dropped:\n{ddl}"
            ));
        }
        // And the server agrees: the wrong-case member is now accepted, which is the
        // exact reverse of the default-facet test.
        session
            .batch(&format!(
                "INSERT INTO {}.`issues` (status, label) VALUES ('ACTIVE', 'x')",
                quote_ident(&database)
            ))
            .await
            .map_err(|e| format!("a caseSensitive:false enum still refused 'ACTIVE': {e}"))?;
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}
