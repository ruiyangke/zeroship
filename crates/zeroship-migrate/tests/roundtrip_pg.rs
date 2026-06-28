//! Migration round-trip harness coverage against a REAL Postgres.
//!
//! Requires the dedicated `zeroship_migrate_test` database on `:5440`; the full PG
//! suite runs serialized with `MIGRATE_REQUIRE_DB=1`.
#![allow(clippy::future_not_send)]

use compio_postgres::Client;
use zeroship_migrate::apply::role::deprovision_migrator;
use zeroship_migrate::{
    migration_id_for_version, Checksum, ChecksumInput, ExecutorConfig, Migration, MigrationFlags,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn mig(version: u64, name: &str, up: String, down: String) -> Migration {
    let flags = MigrationFlags::default();
    Migration {
        version: migration_id_for_version(version),
        name: name.to_string(),
        checksum: Checksum::of(&ChecksumInput {
            up: &up,
            down: Some(&down),
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        up,
        down: Some(down),
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

fn reversible_set(cfg: &ExecutorConfig) -> Vec<Migration> {
    let s = &cfg.project_schema;
    vec![
        mig(
            1,
            "create_teams",
            format!("CREATE TABLE \"{s}\".\"teams\" (id bigint PRIMARY KEY, name text NOT NULL)"),
            format!("DROP TABLE \"{s}\".\"teams\""),
        ),
        mig(
            2,
            "create_members",
            format!("CREATE TABLE \"{s}\".\"members\" (id bigint PRIMARY KEY, team_id bigint NOT NULL)"),
            format!("DROP TABLE \"{s}\".\"members\""),
        ),
        mig(
            3,
            "add_member_email",
            format!("ALTER TABLE \"{s}\".\"members\" ADD COLUMN email text"),
            format!("ALTER TABLE \"{s}\".\"members\" DROP COLUMN email"),
        ),
        mig(
            4,
            "add_member_team_index",
            format!(
                "ALTER TABLE \"{s}\".\"members\" ADD CONSTRAINT members_team_fk \
                 FOREIGN KEY (team_id) REFERENCES \"{s}\".\"teams\" (id); \
                 CREATE INDEX members_team_id_idx ON \"{s}\".\"members\" (team_id)"
            ),
            format!(
                "DROP INDEX \"{s}\".\"members_team_id_idx\"; \
                 ALTER TABLE \"{s}\".\"members\" DROP CONSTRAINT members_team_fk"
            ),
        ),
    ]
}

#[compio::test]
async fn apply_rollback_reapply_is_structurally_clean() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let migrations = reversible_set(&cfg);
    zeroship_migrate::test_support::assert_reversible_replay_pg(&conn, &cfg, &migrations).await;

    teardown(&conn, &cfg).await;
}
