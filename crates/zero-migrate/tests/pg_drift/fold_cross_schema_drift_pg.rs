//! Folding ops that create a SECOND schema produces a snapshot live
//! introspection can never match.
//!
//! `fold_ops` + `snapshot_schema` + `diff_snapshots` is the documented pairing,
//! and the oracle most of this crate's live tests are built on. It holds only
//! while every op stays inside the project schema.
//!
//! `snapshot_schema(conn, name)` asks `pg_namespace WHERE nspname = $1` — one
//! schema, by construction. `fold_ops` records every schema an op creates, and
//! nothing it does adds the PROJECT schema, because no op creates that. So for a
//! migration carrying `createSchema`, the two sides are not merely different,
//! they are DISJOINT:
//!
//!   fold  schemas = [ other ]      (what an op created)
//!   live  schemas = [ project ]    (what the query window admits)
//!
//! `diff_snapshots` then reports `missing: schema other` for a schema that
//! demonstrably exists, and `is_clean()` is false for a database that matches
//! exactly what the migrations authored.
//!
//! THE DIFFER IS UNIFORM; THE SNAPSHOT IS NOT. All three object classes report
//! an expected object absent from `actual` as missing - roles at drift.rs:1759,
//! schemas at 1770, extensions at 1781. That rule is sound for the classes whose
//! live query is CLUSTER-WIDE, which roles and extensions both are: absence is
//! then real evidence. Measured, both behave correctly - a role dropped out of
//! band is reported, and a cluster carrying `plpgsql` against a fold naming no
//! extensions still diffs clean.
//!
//! Schemas are the only class whose query is narrowed to a single name, so they
//! are the only one where absence is not evidence but an artefact of the window.
//! Whether to widen that query - and accept that a cluster-wide namespace list
//! pulls in `pg_catalog`, `information_schema`, every `pg_temp_*`, and every
//! other tenant.s schemas - is a decision about a public API, so this test
//! RECORDS today.s behaviour rather than choosing.
//!
//! It is written to fail if that changes: a fix makes the drift clean, and this
//! file should then assert cleanliness and say why.
//!
//! Scope: no production path inside the engine calls `diff_snapshots`; it is a
//! public API for embedders and the harness this crate's own oracle uses. The
//! cost is therefore a false alarm for an embedder, and a ceiling on what the
//! oracle can cover — `createSchema` cannot be added to `fold_roundtrip_pg.rs`
//! while this holds.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, snapshot_schema, Approval, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, LockMode, MigrationEngine, MigrationIr, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_fold_cross_schema";

fn token(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "foldxs_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn namespace_exists(session: &PgDevSession, name: &str) -> bool {
    session
        .query(
            "SELECT nspname FROM pg_namespace WHERE nspname = $1",
            &[name.into()],
        )
        .await
        .expect("read pg_namespace")
        .len()
        == 1
}

#[compio::test]
async fn a_second_schema_folds_to_a_snapshot_live_introspection_cannot_match() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("proj");
    let other = token("other");
    let policy = support::operator_charter(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let _guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.pg.meta_schema.clone(),
            other.clone(),
        ],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .expect("create the project schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure journal: {error}"))?;

        let doc = serde_json::json!({
            "ir_version": 1,
            "name": "make_other",
            "owner_app": OWNER,
            "ops": [
                { "op": "createSchema", "name": other },
                {
                    "op": "createTable",
                    "name": "t1",
                    "columns": [{ "name": "id", "type": "int", "nullable": false }],
                    "primaryKey": ["id"]
                }
            ]
        })
        .to_string();

        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
        let guard_cfg = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
        let base = fold_ops(&[], SqlDialect::Postgres, &cfg.project_schema, &policy)
            .map_err(|error| format!("fold the empty base: {error}"))?;
        let live = LiveSchema::from_catalog_snapshot(base, OWNER);
        let artifact = author
            .load_and_lower_guarded(&doc, OWNER, &BTreeMap::new(), &live, &guard_cfg)
            .map_err(|error| format!("lower: {error}"))?;
        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &cfg,
                OWNER,
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply: {error}"))?;

        // The premise. Everything the migration authored is really there, so any
        // drift reported below is about the comparison and not the database.
        assert!(
            namespace_exists(&session, &cfg.project_schema).await,
            "the project schema must exist"
        );
        assert!(
            namespace_exists(&session, &other).await,
            "the second schema must exist - the migration just created it"
        );

        let authored: MigrationIr =
            serde_json::from_str(&doc).map_err(|error| format!("parse the IR: {error}"))?;
        let expected = fold_ops(
            &authored.ops,
            SqlDialect::Postgres,
            &cfg.project_schema,
            &policy,
        )
        .map_err(|error| format!("fold the authored ops: {error}"))?;
        let actual = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live schema: {error}"))?;

        // Disjoint, which is the root of it: neither side is a subset of the
        // other, so no amount of tolerance in the comparison rescues this pairing.
        assert_eq!(
            expected.schemas.keys().collect::<Vec<_>>(),
            vec![&other],
            "the fold records the schema an op created, and not the project schema"
        );
        assert_eq!(
            actual.schemas.keys().collect::<Vec<_>>(),
            vec![&cfg.project_schema],
            "the live snapshot records only the schema its query window admits"
        );

        // TODAY: a schema that exists is reported missing. When this is fixed the
        // assertion below fails, and this file should assert `is_clean()` instead.
        let drift = diff_snapshots(&expected, &actual);
        assert!(
            !drift.is_clean(),
            "recording today's behaviour; if the drift is now clean, invert this test"
        );
        assert_eq!(
            drift.missing_objects,
            vec![format!("schema {other}")],
            "the false report names the schema that does exist"
        );
        // And nothing else is wrong: the table the migration created inside the
        // project schema round-trips, so this is specifically the schema window.
        assert!(
            drift.unexpected_objects.is_empty(),
            "nothing unexpected: {:?}",
            drift.unexpected_objects
        );
        assert!(
            drift.altered_objects.is_empty(),
            "nothing altered: {:?}",
            drift.altered_objects
        );
        Ok(())
    }
    .await;

    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema),
            quote_ident(&other)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("fold a cross-schema migration against live PostgreSQL");
}
