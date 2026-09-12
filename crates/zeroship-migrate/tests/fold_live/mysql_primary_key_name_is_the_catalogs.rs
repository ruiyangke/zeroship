//! **MySQL's snapshot must call the primary key what MySQL's catalog calls it.**
//!
//! MySQL does not store a primary-key name. `information_schema.TABLE_CONSTRAINTS`
//! reports every primary key under the fixed name `PRIMARY`, and the server accepts
//! no other: a `CONSTRAINT <x> PRIMARY KEY` clause is parsed and the symbol
//! discarded. So there is exactly one name a MySQL primary key can have, and the
//! server is the only authority on it.
//!
//! The backend's catalog reader nevertheless SYNTHESIZED a different name and put it
//! in the snapshot. It did so to satisfy a predicate in the neutral backend-contract
//! crate that recognised one shipping vendor's convention and no other, which made
//! the synthesis load-bearing: report the catalog's own answer and the predicate
//! stopped recognising the primary key at all.
//!
//! The ORACLE here is the server. This file writes down no expected name; it asks
//! `information_schema` what the constraint is called and requires the shipped
//! `snapshot_schema` to agree. A literal would be a second opinion about the thing
//! under test.
//!
//! REQUIRES `ZERO_MIGRATE_MYSQL_URL` through `require_live_mysql!`: a missing DSN is
//! a failure rather than a green run with no coverage.

use crate::support;

use std::collections::BTreeMap;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zeroship_migrate::apply::backend::MigrationBackend;
use zeroship_migrate::driver::{Bind, SqlSession};
use zeroship_migrate::{
    diff_snapshots, fold_ops, model::ir::Op, resolve_create_table_policy, Approval, ExecutorConfig,
    GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr, SchemaSnapshot,
};
use zeroship_migrate_mysql::MysqlBackend;

const OWNER: &str = "app_mysql_primary_key_name";

/// One table's snapshot, tolerating the qualified key form the snapshot may use.
fn table<'a>(
    snapshot: &'a SchemaSnapshot,
    name: &str,
) -> &'a zeroship_migrate::model::snapshot::TableSnapshot {
    snapshot
        .tables
        .iter()
        .find(|(key, _)| key.as_str() == name || key.ends_with(&format!(".{name}")))
        .map(|(_, t)| t)
        .unwrap_or_else(|| panic!("table {name:?} in snapshot: {:?}", snapshot.tables.keys()))
}

/// The name `information_schema` reports for a table's primary key, and the name the
/// shipped snapshot reader gives the same object, must be one name.
///
/// The index bucket is checked alongside the constraint bucket because the reader
/// stamps both from the same synthesized string, so a fix that corrected only the
/// constraint would leave the index still naming a relation the server has never
/// heard of.
#[compio::test]
async fn the_snapshots_primary_key_name_is_the_one_the_catalog_reports() {
    let url = require_live_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("pkname");
    let cfg = ExecutorConfig::new(
        format!("project_{database}"),
        &database,
        support::no_inject(&database),
    );
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated primary-key-name database");
    session
        .batch(&format!(
            "CREATE TABLE {}.items (id BIGINT NOT NULL, label TEXT, PRIMARY KEY (id))",
            quote_ident(&database)
        ))
        .await
        .expect("create the probe table");

    // The oracle: MySQL's own answer, read from MySQL.
    let rows = session
        .query(
            "SELECT CONSTRAINT_NAME FROM information_schema.TABLE_CONSTRAINTS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_TYPE = 'PRIMARY KEY'",
            &[
                Bind::Text(database.clone()),
                Bind::Text("items".to_string()),
            ],
        )
        .await
        .expect("read the live primary-key constraint name");
    let catalog_name: String = rows
        .first()
        .expect("MySQL reports a primary-key row for a table that has one")
        .try_get("CONSTRAINT_NAME")
        .expect("CONSTRAINT_NAME decodes as text");

    let snapshot = MysqlBackend::new_generic(&session)
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot the live MySQL schema");
    let items = table(&snapshot, "items");

    let folded_constraint = items
        .constraints
        .iter()
        .find(|constraint| constraint.kind == "PRIMARY KEY")
        .expect("the snapshot carries a PRIMARY KEY constraint");
    assert_eq!(
        folded_constraint.name, catalog_name,
        "the snapshot named the primary-key CONSTRAINT {:?}, but MySQL calls it \
         {catalog_name:?}; the backend is reporting a name its own server does not \
         have so that a neutral predicate written for another vendor keeps \
         recognising it",
        folded_constraint.name
    );

    let primary_indexes: Vec<&str> = items
        .indexes
        .iter()
        .filter(|index| index.columns == ["id"] && index.unique)
        .map(|index| index.name.as_str())
        .collect();
    assert_eq!(
        primary_indexes,
        vec![catalog_name.as_str()],
        "the snapshot named the primary key's INDEX differently from what MySQL \
         reports ({catalog_name:?})"
    );
}

/// Apply one IR doc through the REAL MySQL pipeline, returning the RESOLVED ops so
/// the caller folds the exact stream the deploy ran.
async fn apply_doc(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    source: &str,
) -> Result<Vec<Op>, String> {
    let policy = support::no_inject(&cfg.project_schema);
    let authored: MigrationIr =
        serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved test IR: {error}"))?;
    let author = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        &cfg.project_schema,
        OWNER,
        &zeroship_migrate_mysql::DIALECT,
        &policy,
    );
    let guard = GuardConfig::from_policy(
        policy.clone(),
        zeroship_migrate_mysql::DIALECT,
        &cfg.project_schema,
    );
    let artifact = author
        .load_and_lower_guarded(
            &resolved_source,
            OWNER,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &guard,
        )
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    MigrationEngine::new(zeroship_migrate::shipping_vendors())
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &MysqlBackend::new_generic(session),
            cfg,
            "mysql-primary-key-name",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    Ok(resolved.ops)
}

/// A COMPOSITE primary key is the case that puts the name into EMITTED DDL, and it
/// has to survive the round trip.
///
/// A single-column primary key is rendered inline on its own column clause, so the
/// name never appears in the statement at all — a test using one proves the READ
/// side and says nothing about the write side. A composite key is rendered as a
/// table-level `CONSTRAINT <name> PRIMARY KEY (...)`, so this is the only path where
/// the backend's answer is spelled into SQL the server must accept.
///
/// MySQL parses a `CONSTRAINT` symbol on a primary key and discards it, so the name
/// that goes in is not necessarily the name that comes out. That makes the deploy a
/// genuine test rather than a formality: the assertion is a CLEAN structural drift
/// between the offline fold and the introspected server, which is exactly the report
/// that would go permanently red if the two sides disagreed about what the key is
/// called.
#[compio::test]
async fn a_composite_primary_key_deploys_and_folds_to_what_the_server_reports() {
    let url = require_live_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("pkcomp");
    let cfg = ExecutorConfig::new(
        format!("project_{database}"),
        &database,
        support::no_inject(&database),
    );
    let _guard = DatabaseGuard::arm(
        &session,
        [database.clone(), format!("{database}_migrations")],
    );
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated composite-primary-key database");

    let result: Result<(), String> = async {
        let doc = r#"{"ir_version":1,"name":"create_entries","ops":[
            {"op":"createTable","name":"entries","columns":[
                {"name":"tenant_id","type":"int","nullable":false},
                {"name":"entry_id","type":"int","nullable":false},
                {"name":"label","type":"text","nullable":true}
            ],
            "primaryKey":["tenant_id","entry_id"]}
        ]}"#;
        let ops = apply_doc(&session, &cfg, doc).await?;

        // The server accepted the emitted CONSTRAINT clause; now ask it what it
        // actually kept.
        let rows = session
            .query(
                "SELECT CONSTRAINT_NAME FROM information_schema.TABLE_CONSTRAINTS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_TYPE = 'PRIMARY KEY'",
                &[
                    Bind::Text(database.clone()),
                    Bind::Text("entries".to_string()),
                ],
            )
            .await
            .map_err(|error| format!("read the live composite primary-key name: {error}"))?;
        let catalog_name: String = rows
            .first()
            .ok_or("MySQL reports no primary key for the deployed composite table")?
            .try_get("CONSTRAINT_NAME")
            .map_err(|error| format!("CONSTRAINT_NAME decodes as text: {error}"))?;

        let expected = fold_ops(
            zeroship_migrate::shipping_vendors(),
            &ops,
            &zeroship_migrate_mysql::DIALECT,
            &cfg.project_schema,
            &support::no_inject(&cfg.project_schema),
        )
        .map_err(|error| format!("fold the op stream offline: {error}"))?;
        let actual = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the live MySQL schema: {error}"))?;

        let folded_name = table(&expected, "entries")
            .constraints
            .iter()
            .find(|constraint| constraint.kind == "PRIMARY KEY")
            .map(|constraint| constraint.name.clone())
            .ok_or("the fold carries a PRIMARY KEY constraint for the composite table")?;
        if folded_name != catalog_name {
            return Err(format!(
                "the offline fold calls the composite primary key {folded_name:?} while \
                 the server calls it {catalog_name:?}"
            ));
        }

        let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual);
        if drift.is_clean() {
            return Ok(());
        }
        Err(format!(
            "the composite primary key deployed and the server holds exactly what \
             was asked for (its primary key is {catalog_name:?}), but structural \
             drift reported a difference: {drift:#?}"
        ))
    }
    .await;

    result.expect("a composite primary key round-trips through MySQL");
}
