//! **A live table that no backend claims must still report the drift on it.**
//!
//! `apply::drift::introspected_table_vendor` recognises a live catalog read by the
//! evidence only introspection leaves, and every vendor claims provenance on ONE
//! positive marker: PostgreSQL on a column's `ddl_type_override`, MySQL on a column's
//! `text_storage`, SQLite on the table's `stored_create_sql`. A table carrying
//! none of the three matches no vendor, and the ID-default recovery under it is then
//! handed `None` for the dialect - the one entry point in
//! `render::value_format` that takes an optional one.
//!
//! # That shape is LIVE, and this is the fixture that proves it
//!
//! It is tempting to read the no-provenance case as defensive, because two of the
//! three backends can never produce it: PostgreSQL stamps `ddl_type_override` on
//! EVERY column it reads (`zero-migrate-postgres/src/backend/drift_sql.rs` writes
//! `ddl_type_override: Some(format_type)` unconditionally), and SQLite's snapshot is
//! built out of the stored CREATE text it also retains.
//!
//! MySQL is the one that can. `zero-migrate-mysql/src/backend/drift_sql.rs` writes
//! `ddl_type_override: None` and `stored_create_sql: None` unconditionally, so MySQL's
//! whole claim rests on `text_storage`, which `information_schema` populates
//! only for columns that HAVE a character set. A table whose columns are all numeric
//! therefore comes back from a real server with nothing any vendor recognises.
//!
//! `counters` is that table. `labelled_counters` is its twin, identical but for one
//! `note text` column that exists only to hand MySQL its marker back - so the two
//! differ in exactly the fact under test and nothing else.
//!
//! # What is asserted
//!
//! The out-of-band change is the same on both: `AUTO_INCREMENT` dropped from the
//! identity column and a literal `DEFAULT 7` put in its place, which is an ordinary
//! thing for a DBA to do behind the engine's back. The contract is that losing the
//! provenance does not lose the drift: both tables must report a `default` line on
//! `id`, naming the authored `absent` against a live default that still names `7`.
//!
//! The two lines are not required to be spelled the SAME - the unattributed side
//! composes its key across every registered vendor rather than applying MySQL's rules,
//! and the two composers reach the same verdict by different text. Pinning the
//! spelling would pin an internal comparison key; pinning the VERDICT and the
//! faithfulness of the line is the contract.
//!
//! # The instrument is asserted directly
//!
//! The premise of this file is a measured fact about what MySQL's catalog reader
//! leaves behind, and if that ever changes - a reader that starts stamping a
//! `ddl_type_override`, or a fixture that acquires a character column - the test would
//! silently stop measuring the no-provenance path while still passing. So the markers
//! are checked on the introspected snapshot BEFORE the report is asked anything, on
//! both tables and in both directions, and a fixture that no longer isolates the case
//! fails as a BROKEN INSTRUMENT rather than going quietly green.
//!
//! The half a live MySQL server cannot reach - a UUID generator default, which needs a
//! character-typed column on MySQL and so hands the marker back - is covered at the
//! unit boundary by `apply::drift::unattributed_snapshot_tests`.
//!
//! REQUIRES `ZERO_MIGRATE_MYSQL_URL` through `require_live_mysql!`: a missing DSN is a
//! failure rather than a green run with no coverage.

use crate::support;

use std::collections::BTreeMap;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::snapshot::TableSnapshot;
use zero_migrate::{
    diff_snapshots, fold_ops, model::ir::Op, resolve_create_table_policy, Approval, ExecutorConfig,
    GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, SchemaSnapshot, StructuralDrift,
};
use zero_migrate_mysql::MysqlBackend;

const OWNER: &str = "app_drift_unattributed_snapshot";

/// The table no vendor can claim, and the twin that hands MySQL its marker back.
const BARE: &str = "counters";
const MARKED: &str = "labelled_counters";

fn cfg_for(database: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{database}"),
        database,
        support::no_inject(database),
    )
}

fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(table, owner)| ((*table).to_string(), (*owner).to_string()))
        .collect()
}

/// Apply one IR doc through the REAL MySQL pipeline, returning the RESOLVED ops so the
/// caller folds the exact stream the engine deployed.
async fn apply_doc(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    source: &str,
    registry: &BTreeMap<String, String>,
    live: &LiveSchema,
) -> Result<Vec<Op>, String> {
    let policy = support::no_inject(&cfg.project_schema);
    let authored: MigrationIr =
        serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved test IR: {error}"))?;
    let author = IrAuthor::new(
        &cfg.project_schema,
        OWNER,
        &zero_migrate_mysql::DIALECT,
        &policy,
    );
    let guard = GuardConfig::from_policy(policy.clone(), zero_migrate_mysql::DIALECT);
    let artifact = author
        .load_and_lower_guarded(&resolved_source, OWNER, registry, live, &guard)
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &MysqlBackend::new_generic(session),
            cfg,
            "drift-unattributed-snapshot",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    Ok(resolved.ops)
}

fn table<'s>(snapshot: &'s SchemaSnapshot, table: &str) -> Result<&'s TableSnapshot, String> {
    snapshot
        .tables
        .iter()
        .find(|(name, _)| name.as_str() == table || name.ends_with(&format!(".{table}")))
        .map(|(_, t)| t)
        .ok_or_else(|| format!("no table {table} in the snapshot"))
}

/// Every provenance marker a registered vendor claims on, as readable lines.
fn markers(snapshot: &SchemaSnapshot, name: &str) -> String {
    match table(snapshot, name) {
        Err(error) => error,
        Ok(t) => format!(
            "stored_create_sql={:?}\n      {}",
            t.stored_create_sql.as_deref(),
            t.columns
                .iter()
                .map(|c| format!(
                    "{}: ddl_type_override={:?} text_storage={:?}",
                    c.name, c.ddl_type_override, c.text_storage
                ))
                .collect::<Vec<_>>()
                .join("\n      ")
        ),
    }
}

/// Whether any registered vendor would claim this introspected table.
///
/// The three positive markers `introspected_table_vendor` folds over, read off the
/// snapshot rather than through the private helper itself.
fn any_vendor_claims(snapshot: &SchemaSnapshot, name: &str) -> Result<bool, String> {
    let t = table(snapshot, name)?;
    Ok(t.stored_create_sql.is_some()
        || t.columns.iter().any(|c| c.ddl_type_override.is_some())
        || t.columns.iter().any(|c| c.text_storage.is_some()))
}

/// The one `default` line for a table's `id` column, or `None`.
///
/// The table match is ANCHORED on a schema separator rather than a bare suffix, and
/// that is load-bearing here: `labelled_counters` ends with `counters`, so a suffix
/// match hands the twin's line back when the unclaimed table has none - which is
/// exactly the failure this file exists to catch, silently reported as a pass. It was
/// measured that way before the anchor went in.
fn default_line<'d>(
    drift: &'d StructuralDrift,
    name: &str,
) -> Option<&'d zero_migrate::apply::drift::AlteredObject> {
    drift.altered_objects.iter().find(|a| {
        (a.table == name || a.table.ends_with(&format!(".{name}")))
            && a.object == "column id"
            && a.field == "default"
    })
}

/// **THE CONTRACT.** A live MySQL table whose columns are all numeric carries no
/// backend provenance, and the ID-default drift on it must still be reported.
#[compio::test]
async fn live_mysql_reports_a_default_change_on_a_table_no_backend_claims() {
    let url = require_live_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("unattrib");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated unattributed database");

    let result: Result<(), String> = async {
        let create = r#"{"ir_version":1,"name":"create_counters","ops":[
            {"op":"createTable","name":"counters","columns":[
                {"name":"id","type":"bigInt","nullable":false,"identity":{"always":false}},
                {"name":"n","type":"int","nullable":false}
            ],
            "primaryKey":["id"]},
            {"op":"createTable","name":"labelled_counters","columns":[
                {"name":"id","type":"bigInt","nullable":false,"identity":{"always":false}},
                {"name":"n","type":"int","nullable":false},
                {"name":"note","type":"text","nullable":true}
            ],
            "primaryKey":["id"]}
        ]}"#;
        let ops = apply_doc(
            &session,
            &cfg,
            create,
            &registry(&[]),
            &LiveSchema::default(),
        )
        .await?;

        let expected = fold_ops(
            &ops,
            &zero_migrate_mysql::DIALECT,
            &cfg.project_schema,
            &support::no_inject(&cfg.project_schema),
        )
        .map_err(|error| format!("fold the op stream offline: {error}"))?;

        // THE CONTROL: an untouched deploy that already drifted would make the line
        // below say nothing about the change.
        let untouched = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the untouched schema: {error}"))?;
        let clean = diff_snapshots(&expected, &untouched);
        if !clean.is_clean() {
            return Err(format!(
                "the tables were deployed by the engine and left alone, yet drift \
                 reported a difference: {clean:#?}"
            ));
        }

        // THE INSTRUMENT, measured off the server before the report is asked anything.
        // `counters` is the fixture only because MySQL leaves it unclaimable; if that
        // ever stops being true this file must say so rather than pass.
        if any_vendor_claims(&untouched, BARE)? {
            return Err(format!(
                "BROKEN INSTRUMENT: MySQL's catalog read of {BARE} carries a provenance \
                 marker, so this test no longer reaches the path it names. Every column \
                 is numeric and none should have one:\n      {}",
                markers(&untouched, BARE)
            ));
        }
        if !any_vendor_claims(&untouched, MARKED)? {
            return Err(format!(
                "BROKEN INSTRUMENT: {MARKED} exists to be the CLAIMED twin - its `note` \
                 column is character-typed, so MySQL should recognise it - and no \
                 marker is present:\n      {}",
                markers(&untouched, MARKED)
            ));
        }

        // OUT OF BAND, identically on both: AUTO_INCREMENT dropped, a literal default
        // put in its place.
        for name in [BARE, MARKED] {
            session
                .batch(&format!(
                    "ALTER TABLE {}.{} MODIFY COLUMN `id` BIGINT NOT NULL DEFAULT 7",
                    quote_ident(&database),
                    quote_ident(name)
                ))
                .await
                .map_err(|error| format!("drop {name}'s auto_increment out of band: {error}"))?;
        }

        let actual = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the changed schema: {error}"))?;
        // The marker state must survive the ALTER, or the comparison below is not the
        // one this file names.
        if any_vendor_claims(&actual, BARE)? {
            return Err(format!(
                "BROKEN INSTRUMENT: the out-of-band ALTER gave {BARE} a provenance \
                 marker:\n      {}",
                markers(&actual, BARE)
            ));
        }
        let drift = diff_snapshots(&expected, &actual);

        // THE ASSERTION. The unclaimed table first, then its claimed twin - the two
        // must reach the same verdict, which is what makes the missing provenance the
        // measured variable rather than an untested difference.
        for name in [BARE, MARKED] {
            let Some(line) = default_line(&drift, name) else {
                return Err(format!(
                    "the live default on {name}.id changed out of band and drift \
                     reported no default line for it. Drift: {drift:#?}"
                ));
            };
            if line.expected != "absent" {
                return Err(format!(
                    "the authored side of {name}.id is an identity column with no \
                     database default, which the ID-default surface spells `absent`, \
                     and the line reads {:?}",
                    line.expected
                ));
            }
            // Faithfulness: whatever key the comparison composed, the line has to name
            // the default the server now holds.
            if line.actual == "absent" || !line.actual.contains('7') {
                return Err(format!(
                    "the default line for {name}.id must name the live `DEFAULT 7` that \
                     replaced the identity, and it reads {:?}",
                    line.actual
                ));
            }
        }
        Ok(())
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}
