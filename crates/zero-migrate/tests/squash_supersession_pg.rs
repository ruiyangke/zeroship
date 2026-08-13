//! `squash` against live `PostgreSQL`: the all-or-none rule and what it writes.
//!
//! `squash` is a public export of this crate (`lib.rs`) and it is a WRITER — it
//! takes the project advisory lock and appends to the journal that decides what
//! has been applied. It is not reachable from the CLI or the Node addon, so an
//! embedder using the Rust crate is its only caller, and until now its only test
//! was a `SQLite` routing negative (`sqlite_apply.rs`) proving the backend refuses
//! it. Nothing exercised the operation itself against a real server.
//!
//! The module's central safety claim is the ALL-OR-NONE rule: a squash may record
//! a supersession only when every superseded version is net-applied. A partial
//! overlap is an inconsistent state and must be refused. That rule is the whole
//! reason squash is safe to run on a live database — a squash recorded over a
//! partially-applied prefix would mark migrations satisfied that never ran, and
//! the executor would then skip them forever.
//!
//! So each refusal here asserts the JOURNAL IS UNCHANGED, not merely that an
//! error came back. A refusal that had already written its row would return the
//! same error and leave the database in exactly the state the rule exists to
//! prevent.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`, and it announces the skip rather than
//! reporting the same count either way.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{
    rollback, LockMode, RollbackError, RollbackOptions, RollbackRequest, RollbackTarget,
};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, squash, Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    MigrationEngine, PostgresBackend, SqlDialect, SquashError,
};

const OWNER: &str = "app_squash_supersession_pg";

fn token(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "squash_pg_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn policy(schema: &str) -> zero_migrate::EffectivePolicy {
    support::operator_charter(schema)
}

/// One `CREATE TABLE` migration, authored as IR so the versions are real.
fn create_doc(table: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": format!("create_{table}"),
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": table,
                "columns": [
                    { "name": "id", "type": "int", "nullable": false }
                ],
                "primaryKey": ["id"]
            }
        ]
    })
    .to_string()
}

/// Author + apply one IR document, returning the DDL migrations it journaled.
async fn apply_doc(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
    history: &mut Vec<Op>,
) -> Result<Vec<Migration>, String> {
    let backend = PostgresBackend::new_generic(session);
    let pol = policy(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &pol);
    let guard = GuardConfig::from_policy(pol.clone(), SqlDialect::Postgres);
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &pol)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let artifact = author
        .load_and_lower_guarded(ir, OWNER, &BTreeMap::new(), &live, &guard)
        .map_err(|error| format!("load and lower the guarded plan: {error}"))?;
    let authored: zero_migrate::MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse the authored IR: {error}"))?;
    history.extend(authored.ops);
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::None,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply the authored plan on PostgreSQL: {error}"))?;
    Ok(artifact
        .plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(m) => Some(m.clone()),
            _ => None,
        })
        .collect())
}

/// A squash migration superseding `supersedes`, with a plausible combined `up`.
fn squash_migration(name: &str, up: &str, supersedes: Vec<MigrationId>) -> Migration {
    let flags = MigrationFlags::default();
    let supersedes_refs: Vec<MigrationId> = supersedes.clone();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: OWNER,
        depends_on: &[],
        supersedes: &supersedes_refs,
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum,
        flags,
        owner_app: OWNER.to_string(),
        depends_on: Vec::new(),
        supersedes,
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

/// Every journal event, as `(version, event_kind, kind)`, in commit order.
async fn journal_events(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
) -> Result<Vec<(String, String, Option<String>)>, String> {
    let meta = quote_ident(&cfg.pg.meta_schema);
    let rows = session
        .query(
            &format!(
                "SELECT version, event_kind, kind FROM {meta}.schema_migrations \
                  ORDER BY event_seq"
            ),
            &[],
        )
        .await
        .map_err(|error| format!("read the journal: {error}"))?;
    rows.iter()
        .map(|row| {
            let version: String = row
                .try_get("version")
                .map_err(|e| format!("version: {e}"))?;
            let event_kind: String = row
                .try_get("event_kind")
                .map_err(|e| format!("event_kind: {e}"))?;
            let kind: Option<String> = row.try_get("kind").map_err(|e| format!("kind: {e}"))?;
            Ok((version, event_kind, kind))
        })
        .collect()
}

/// The recorded supersession edges, as `(squash_version, superseded_version)`.
async fn supersession_edges(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
) -> Result<Vec<(String, String)>, String> {
    let meta = quote_ident(&cfg.pg.meta_schema);
    let rows = session
        .query(
            // Named columns, not `*`: the table also carries a timestamp this
            // driver has no reason to decode here, and `*` makes reading the two
            // columns under test depend on every other column's type.
            &format!(
                "SELECT squash_version, superseded_version FROM {meta}.schema_migrations_supersedes"
            ),
            &[],
        )
        .await
        .map_err(|error| format!("read the supersession edges: {error}"))?;
    rows.iter()
        .map(|row| {
            let version: String = row
                .try_get("squash_version")
                .map_err(|e| format!("squash_version: {e}"))?;
            let superseded: String = row
                .try_get("superseded_version")
                .map_err(|e| format!("superseded_version: {e}"))?;
            Ok((version, superseded))
        })
        .collect()
}

/// The project schema's live tables, so a refusal can be checked against what is
/// actually there rather than against what the journal claims.
async fn live_tables(session: &PgDevSession, schema: &str) -> Result<Vec<String>, String> {
    let rows = session
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = $1 \
              ORDER BY table_name",
            &[schema.into()],
        )
        .await
        .map_err(|error| format!("list live tables: {error}"))?;
    rows.iter()
        .map(|row| {
            row.try_get("table_name")
                .map_err(|e| format!("table_name: {e}"))
        })
        .collect()
}

/// Set up a schema with `tables` applied, and hand the caller their versions.
async fn applied_project(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    tables: &[&str],
) -> Result<Vec<MigrationId>, String> {
    let backend = PostgresBackend::new_generic(session);
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| format!("ensure migration journal: {error}"))?;
    let mut history: Vec<Op> = Vec::new();
    let mut versions = Vec::new();
    for table in tables {
        let applied = apply_doc(session, cfg, &create_doc(table), &mut history).await?;
        versions.extend(applied.into_iter().map(|m| m.version));
    }
    Ok(versions)
}

#[compio::test]
async fn squashing_a_fully_applied_prefix_records_a_supersession_and_is_idempotent() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("all");
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        let versions = applied_project(&session, &cfg, &["t1", "t2"]).await?;
        assert_eq!(versions.len(), 2, "two migrations must have been applied");
        let before = journal_events(&session, &cfg).await?;

        let s = squash_migration(
            "squash_t1_t2",
            &format!(
                "CREATE TABLE {}.t1 (id int PRIMARY KEY); \
                 CREATE TABLE {}.t2 (id int PRIMARY KEY);",
                quote_ident(&cfg.project_schema),
                quote_ident(&cfg.project_schema)
            ),
            versions.clone(),
        );

        let outcome = squash(&backend, &cfg, &s, "operator")
            .await
            .map_err(|error| format!("squash a fully-applied prefix: {error}"))?;
        assert!(
            !outcome.already_present,
            "the first squash records a new supersession"
        );
        assert_eq!(
            outcome.superseded.len(),
            2,
            "both versions must be reported superseded"
        );

        // The journal is APPEND-ONLY: the superseded events survive, and the
        // squash arrives as one new `completed` row stamped `squash`.
        let after = journal_events(&session, &cfg).await?;
        for event in &before {
            assert!(
                after.contains(event),
                "squash must not remove the events it supersedes; lost {event:?}"
            );
        }
        let squash_rows: Vec<_> = after
            .iter()
            .filter(|(version, _, _)| version == s.version.as_str())
            .collect();
        assert_eq!(
            squash_rows.len(),
            1,
            "exactly one squash event, got {squash_rows:?}"
        );
        assert_eq!(
            squash_rows[0].1, "applied",
            "the squash event is an applied event"
        );
        assert_eq!(
            squash_rows[0].2.as_deref(),
            Some("squash"),
            "the squash event must be stamped kind = squash"
        );

        // The supersession edges are what let a fresh build skip the prefix.
        let mut edges = supersession_edges(&session, &cfg).await?;
        edges.sort();
        let mut expected: Vec<(String, String)> = versions
            .iter()
            .map(|v| (s.version.as_str().to_string(), v.as_str().to_string()))
            .collect();
        expected.sort();
        assert_eq!(edges, expected, "one edge per superseded version");

        // Re-squashing is idempotent, and writes nothing further.
        let again = squash(&backend, &cfg, &s, "operator")
            .await
            .map_err(|error| format!("re-squash the same migration: {error}"))?;
        assert!(
            again.already_present,
            "a re-squash must report the supersession as already present"
        );
        assert_eq!(
            journal_events(&session, &cfg).await?,
            after,
            "a re-squash must append nothing"
        );
        Ok(())
    }
    .await;

    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("squash a fully-applied prefix on live PostgreSQL");
}

#[compio::test]
async fn squashing_a_partially_applied_prefix_is_refused_and_writes_nothing() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("partial");
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        // Only `t1` is applied. The squash claims to supersede `t1` AND a version
        // that was never applied at all, which is the inconsistent middle the
        // all-or-none rule exists to refuse.
        let applied = applied_project(&session, &cfg, &["t1"]).await?;
        let before = journal_events(&session, &cfg).await?;
        let before_edges = supersession_edges(&session, &cfg).await?;

        let mut supersedes = applied.clone();
        supersedes.push(MigrationId::generate());
        let s = squash_migration(
            "squash_partial",
            &format!(
                "CREATE TABLE {}.t1 (id int PRIMARY KEY);",
                quote_ident(&cfg.project_schema)
            ),
            supersedes,
        );

        let error = squash(&backend, &cfg, &s, "operator")
            .await
            .expect_err("a partial overlap must be refused");
        match error {
            SquashError::PartialOverlap { applied, total, .. } => {
                assert_eq!(applied, 1, "one of the superseded versions was applied");
                assert_eq!(total, 2, "of two claimed");
            }
            other => return Err(format!("expected PartialOverlap, got: {other}")),
        }

        // The refusal must be a REFUSAL, not a failure after the write. A squash
        // recorded here would mark a never-applied migration satisfied forever.
        assert_eq!(
            journal_events(&session, &cfg).await?,
            before,
            "a refused squash must append no journal event"
        );
        assert_eq!(
            supersession_edges(&session, &cfg).await?,
            before_edges,
            "a refused squash must record no supersession edge"
        );
        Ok(())
    }
    .await;

    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("refuse a partially-applied squash on live PostgreSQL");
}

#[compio::test]
async fn squashing_an_unapplied_prefix_is_refused_as_not_applied() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("none");
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;
        let before = journal_events(&session, &cfg).await?;

        // NONE applied. This is the fresh-database path, which belongs to `apply`
        // and runs `S.up`; reaching it through `squash` must not record anything,
        // because nothing is there to supersede.
        let s = squash_migration(
            "squash_nothing",
            &format!(
                "CREATE TABLE {}.t1 (id int PRIMARY KEY);",
                quote_ident(&cfg.project_schema)
            ),
            vec![MigrationId::generate(), MigrationId::generate()],
        );
        let error = squash(&backend, &cfg, &s, "operator")
            .await
            .expect_err("an unapplied prefix must be refused");
        match error {
            SquashError::NotAllApplied { applied, total, .. } => {
                assert_eq!(applied, 0, "none of the superseded versions were applied");
                assert_eq!(total, 2, "of two claimed");
            }
            other => return Err(format!("expected NotAllApplied, got: {other}")),
        }
        assert_eq!(
            journal_events(&session, &cfg).await?,
            before,
            "a refused squash must append no journal event"
        );

        // A squash that supersedes nothing is refused before any of that, and is
        // the control for the two arms above: it proves `squash` can reject on the
        // migration alone, so their refusals are about applied STATE and not about
        // the migration being malformed.
        let empty = squash_migration("squash_empty", "SELECT 1", Vec::new());
        match squash(&backend, &cfg, &empty, "operator")
            .await
            .expect_err("a squash superseding nothing must be refused")
        {
            SquashError::NoSupersedes { .. } => {}
            other => return Err(format!("expected NoSupersedes, got: {other}")),
        }
        Ok(())
    }
    .await;

    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("refuse an unapplied squash on live PostgreSQL");
}

/// A squash that CAN be unwound, for the control arm below.
fn reversible_squash(name: &str, up: &str, down: &str, supersedes: Vec<MigrationId>) -> Migration {
    let mut m = squash_migration(name, up, supersedes);
    m.down = Some(down.to_string());
    m.checksum = Checksum::of(&ChecksumInput {
        up,
        down: Some(down),
        flags: &m.flags,
        owner_app: OWNER,
        depends_on: &[],
        supersedes: &m.supersedes,
        preconditions: &[],
    });
    m
}

// ---------------------------------------------------------------------------
// A rollback may not FORCE-SKIP an irreversible squash.
//
// `rollback` refuses to cross a migration with `down: None`, and offers
// `force + backup_acknowledged` as the override, which SKIPS that migration.
// Skipping any ordinary migration only forgoes its own undo. Skipping a SQUASH
// is different in kind: `superseded_versions` honours the edges of a NET-APPLIED
// squash, and that is deliberate — its doc says rolling the squash back is what
// releases its supersession. A skip keeps the squash net-applied while this same
// rollback unwinds everything it supersedes.
//
// Measured before the fix, on a live server:
//
//   apply v1,v2            tables = t1,t2
//   squash S over [v1,v2]  2 edges recorded
//   rollback All + force   rolled_back = 2, skipped = 1 (S), tables = NONE
//   redeploy t1            Ok(applied: [], skipped: [v1]), tables = STILL NONE
//
// The tables are gone, the journal says they are covered by a squash, and apply
// reports SUCCESS having created nothing. There is no state left from which the
// normal path recovers, and nothing in the output says so.
//
// So the skip is refused ahead of the force check: this is not a data-loss trade
// an operator can acknowledge their way past, because the damage is to what the
// journal MEANS rather than to any one table.
// ---------------------------------------------------------------------------
#[compio::test]
async fn a_rollback_may_not_force_skip_an_irreversible_squash() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("noskip");
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history: Vec<Op> = Vec::new();
        let mut applied: Vec<Migration> = Vec::new();
        for table in ["t1", "t2"] {
            applied.extend(apply_doc(&session, &cfg, &create_doc(table), &mut history).await?);
        }
        let versions: Vec<MigrationId> = applied.iter().map(|m| m.version.clone()).collect();

        let combined = format!(
            "CREATE TABLE {}.t1 (id int PRIMARY KEY);",
            quote_ident(&cfg.project_schema)
        );
        let s = squash_migration("squash_all", &combined, versions.clone());
        squash(&backend, &cfg, &s, "operator")
            .await
            .map_err(|error| format!("record the squash: {error}"))?;

        let before_events = journal_events(&session, &cfg).await?;
        let before_edges = supersession_edges(&session, &cfg).await?;
        let before_tables = live_tables(&session, &cfg.project_schema).await?;
        assert_eq!(before_tables, vec!["t1", "t2"], "both tables start present");

        // The set an operator still has on disk right after a squash.
        let mut set = applied.clone();
        set.push(s.clone());
        let guard = guard_for(&GuardConfig::from_policy(
            policy(&cfg.project_schema),
            SqlDialect::Postgres,
        ));
        let forced = RollbackRequest::new(RollbackTarget::All).with_options(RollbackOptions {
            force: true,
            backup_acknowledged: true,
        });

        let error = rollback(
            &backend,
            &cfg,
            &forced,
            &set,
            Approval::Approved,
            "operator",
            guard.as_ref(),
        )
        .await
        .expect_err("force-skipping a squash must be refused");
        match error {
            RollbackError::IrreversibleSquash { superseded, .. } => {
                assert_eq!(superseded, 2, "the refusal must name how much it covers");
            }
            other => return Err(format!("expected IrreversibleSquash, got: {other}")),
        }

        // Refused BEFORE any down ran: this is the whole point, since the damage
        // is not recoverable once the covered versions are unwound.
        assert_eq!(
            live_tables(&session, &cfg.project_schema).await?,
            before_tables,
            "a refused rollback must not have dropped anything"
        );
        assert_eq!(
            journal_events(&session, &cfg).await?,
            before_events,
            "a refused rollback must append no journal event"
        );
        assert_eq!(
            supersession_edges(&session, &cfg).await?,
            before_edges,
            "a refused rollback must not disturb the supersession edges"
        );

        Ok(())
    }
    .await;

    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("refuse to force-skip an irreversible squash on live PostgreSQL");
}

// ---------------------------------------------------------------------------
// CONTROL for the test above. The refusal keys on `down: None` TOGETHER WITH
// `supersedes`, not on being a squash: a squash that can reverse itself is
// rolled back normally.
//
// It needs its own schema. Sharing one with the test above would leave that
// test's irreversible squash net-applied and absent from this set, and rollback
// refuses that ("applied but absent from the supplied set") long before reaching
// the question being asked.
//
// Without this arm, the refusal above would also pass if `force` had stopped
// working, or if rollback had stopped rolling anything back at all.
// ---------------------------------------------------------------------------
#[compio::test]
async fn a_squash_that_can_reverse_itself_still_rolls_back_under_force() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("revsquash");
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history: Vec<Op> = Vec::new();
        let mut applied: Vec<Migration> = Vec::new();
        for table in ["t1", "t2"] {
            applied.extend(apply_doc(&session, &cfg, &create_doc(table), &mut history).await?);
        }
        let versions: Vec<MigrationId> = applied.iter().map(|m| m.version.clone()).collect();

        let quoted = quote_ident(&cfg.project_schema);
        let reversible = reversible_squash(
            "squash_reversible",
            &format!("CREATE TABLE {quoted}.t1 (id int PRIMARY KEY);"),
            &format!("DROP TABLE IF EXISTS {quoted}.t2; DROP TABLE IF EXISTS {quoted}.t1;"),
            versions,
        );
        squash(&backend, &cfg, &reversible, "operator")
            .await
            .map_err(|error| format!("record the reversible squash: {error}"))?;

        let mut set = applied.clone();
        set.push(reversible.clone());
        let guard = guard_for(&GuardConfig::from_policy(
            policy(&cfg.project_schema),
            SqlDialect::Postgres,
        ));
        let outcome = rollback(
            &backend,
            &cfg,
            // Steps(1) — the squash alone. `All` would run the squash's own down
            // first and then each superseded version's engine-generated down over
            // tables the squash had already dropped, which is a question about
            // authoring a squash's down, not about the refusal under test.
            &RollbackRequest::new(RollbackTarget::Steps(1)).with_options(RollbackOptions {
                force: true,
                backup_acknowledged: true,
            }),
            &set,
            Approval::Approved,
            "operator",
            guard.as_ref(),
        )
        .await
        .map_err(|error| format!("a reversible squash must roll back: {error}"))?;

        assert!(
            outcome
                .rolled_back
                .contains(&reversible.version.as_str().to_string()),
            "the reversible squash must be among the rolled-back versions, got {:?}",
            outcome.rolled_back
        );
        assert!(
            live_tables(&session, &cfg.project_schema).await?.is_empty(),
            "the reversing down must actually have run"
        );
        Ok(())
    }
    .await;

    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&cfg.project_schema),
            quote_ident(&cfg.pg.meta_schema)
        ))
        .await
        .expect("drop the test schemas");
    work.expect("roll back a reversible squash on live PostgreSQL");
}
