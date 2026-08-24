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

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::{Bind, SqlSession};
use zero_migrate::{ExecutorConfig, SchemaSnapshot};
use zero_migrate_mysql::MysqlBackend;

/// One table's snapshot, tolerating the qualified key form the snapshot may use.
fn table<'a>(
    snapshot: &'a SchemaSnapshot,
    name: &str,
) -> &'a zero_migrate::model::snapshot::TableSnapshot {
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
