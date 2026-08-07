//! A PostgreSQL identifier truncation can no longer make the journal lie.
//!
//! PostgreSQL caps identifiers at 63 bytes (NAMEDATALEN) and truncates anything longer
//! with only a NOTICE. The engine keeps the AUTHORED name while the catalog keeps the
//! TRUNCATED one. That used to make a guarded drop probe the AUTHORED name against the
//! INTROSPECTED snapshot, never match the truncated catalog name, decide the work was
//! already done, skip the statement, and journal it COMPLETED while the object survived.
//!
//! The hazard is now closed at two seams, and this suite exercises both against a live
//! server:
//!
//! - LOWER refuses the over-long authored name, so no such statement is ever rendered.
//!   Commit ddd679d bounded the name at the LOAD gate only; `IrAuthor::lower`,
//!   `lower_steps` and `lower_plan` now run the same bound, because they are public
//!   entry points no caller is obliged to reach through the load gate.
//! - The executor's existence PROBE refuses it too, because `Migration::existence_guard`
//!   is a public field on a struct a consumer can build directly. An `ifExists` miss on
//!   an over-long name whose TRUNCATED spelling is live is a lie, and the probe now says
//!   so instead of returning the satisfied no-op.
//!
//! The remedy is the last assertion: name the constraint as the catalog holds it, and
//! the drop applies, the object goes, and the journal tells the truth.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

mod support;

use support::PgDevSession;

use zero_migrate::model::ir::ExistenceGuard;
use zero_migrate::model::probe::{GuardDir, GuardProbe};
use zero_migrate::render::existence_probe::{decide, GuardVerdict};
use zero_migrate::{
    apply, snapshot_schema, Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationIr, Op, Phase,
    SqlDialect,
};

/// PostgreSQL's NAMEDATALEN-derived identifier bound, in bytes.
const MAX: usize = 63;

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(
        format!("prj_{tok}"),
        format!("proj_{tok}"),
        support::no_inject(&format!("proj_{tok}")),
    );
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
}

async fn drop_schemas(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

/// The UNIQUE constraint names the catalog actually holds for `schema.t`.
async fn unique_constraint_names(session: &PgDevSession, schema: &str) -> Vec<String> {
    use zero_migrate::driver::SqlSession;
    session
        .query(
            "SELECT con.conname FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = 't' AND con.contype = 'u' \
             ORDER BY con.conname",
            &[schema.into()],
        )
        .await
        .expect("read pg_constraint")
        .iter()
        .map(|row| row.try_get::<_, String>("conname").expect("decode conname"))
        .collect()
}

fn drop_constraint_ir(name: &str) -> MigrationIr {
    MigrationIr {
        ir_version: 2,
        name: format!("drop_constraint_{}", name.len()),
        owner_app: "app_test".into(),
        ops: vec![Op::DropConstraint {
            table: "t".into(),
            name: name.into(),
            schema: None,
            existence_guard: Some(ExistenceGuard::IfExists),
        }],
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// A guarded drop of a name PostgreSQL truncated is refused at lower and at the probe,
/// and the remedy - naming the constraint as the catalog holds it - really removes it.
///
/// Asserted end to end against a live server: the catalog holds the truncated name,
/// lower refuses the authored one, a hand-built probe over the authored one fails closed
/// naming the truncated spelling, and the truncated-name drop applies, journals
/// Completed, and leaves `pg_constraint` empty of it.
#[compio::test]
async fn a_truncated_constraint_name_can_no_longer_make_a_guarded_drop_journal_a_lie() {
    use zero_migrate::driver::SqlSession;

    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // One byte over NAMEDATALEN, all ASCII, so bytes and characters agree and the
    // truncation is exactly the first 63 bytes.
    let authored = format!("zm_truncated_constraint_{}", "x".repeat(40));
    assert_eq!(
        authored.len(),
        MAX + 1,
        "the authored name must be one byte over the PostgreSQL cap"
    );
    let truncated: String = authored.chars().take(MAX).collect();

    session
        .batch(&format!(
            "CREATE TABLE \"{s}\".t (c text NOT NULL); \
             ALTER TABLE \"{s}\".t ADD CONSTRAINT \"{authored}\" UNIQUE (c)",
            s = cfg.project_schema
        ))
        .await
        .expect("create the table and its over-long constraint");

    // The server truncated with only a NOTICE: the catalog holds a name the authored
    // one is never equal to.
    let before = unique_constraint_names(&session, &cfg.project_schema).await;
    assert_eq!(
        before,
        vec![truncated.clone()],
        "PostgreSQL must have truncated the authored name to {MAX} bytes"
    );
    assert!(
        !before.contains(&authored),
        "the authored 64-byte name is not what the catalog holds"
    );

    let author = IrAuthor::new(
        &cfg.project_schema,
        "app_test",
        SqlDialect::Postgres,
        &support::no_inject(&cfg.project_schema),
    );

    // The lower seam. No statement carrying the authored name is ever rendered, so the
    // whole downstream chain - the guard probe, the skip, the journalled completion -     // is unreachable from a lowered plan.
    let refusal = author
        .lower(&drop_constraint_ir(&authored), &LiveSchema::default())
        .expect_err("lower must refuse a name PostgreSQL cannot hold")
        .to_string();
    assert!(
        refusal.contains("truncates identifiers"),
        "lower was refused for the wrong reason: {refusal}"
    );

    // The probe backstop, which does not depend on lowering having run: a consumer can
    // build a Migration carrying its own existence guard. An ifExists miss on the
    // authored name is a lie whenever the truncated spelling is live.
    let live = snapshot_schema(&session, &cfg.project_schema)
        .await
        .expect("snapshot the live schema");
    let probe = GuardProbe::Constraint {
        schema: cfg.project_schema.clone(),
        table: "t".into(),
        name: authored.clone(),
        direction: GuardDir::IfExists,
        expect_kind: None,
        expect_definition: None,
    };
    match decide(&probe, &live, SqlDialect::Postgres) {
        GuardVerdict::FailDrift(divergence) => assert_eq!(
            divergence.actual, truncated,
            "the verdict must name the truncated spelling the catalog holds"
        ),
        verdict => panic!(
            "the guard must refuse rather than read the drop as already done, got \
             {verdict:?}"
        ),
    }

    // The remedy: name the constraint as the catalog holds it. That drop lowers,
    // applies, and the journal and the catalog agree afterwards.
    let migrations = author
        .lower(&drop_constraint_ir(&truncated), &LiveSchema::default())
        .expect("the catalog's own spelling lowers");
    assert_eq!(migrations.len(), 1, "one migration lowers from one op");
    let remedy_probe = migrations[0]
        .existence_guard
        .as_ref()
        .expect("the guarded drop carries a probe");
    assert_eq!(
        decide(remedy_probe, &live, SqlDialect::Postgres),
        GuardVerdict::RunBare,
        "the catalog's own spelling matches, so the drop runs"
    );

    let out = apply(&session, &cfg, &migrations, Approval::Approved, "app_test")
        .await
        .expect("the guarded drop applies");
    assert_eq!(
        out.applied.len(),
        1,
        "apply reports the migration applied: {out:?}"
    );

    let journal = zero_migrate::applied(&session, &cfg)
        .await
        .expect("read the journal");
    assert!(
        journal
            .iter()
            .any(|e| e.version == out.applied[0] && e.phase == Phase::Completed),
        "the journal records the drop as completed: {journal:?}"
    );

    // The journal says the constraint was dropped, and now the catalog agrees.
    let after = unique_constraint_names(&session, &cfg.project_schema).await;
    assert!(
        after.is_empty(),
        "the constraint the journal says was dropped must really be gone: {after:?}"
    );

    drop_schemas(&session, &cfg).await;
}
