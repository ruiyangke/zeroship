#![allow(unsafe_code)]

#[path = "../../runtime/tests/support/node_realworld.rs"]
mod node_realworld;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use zeroship_migrate::render::step::PlanStep;
use zeroship_migrate::{
    diff_snapshots, fold_ops, ApplyError, Approval, Checksum, ChecksumInput,
    EngineError, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, Migration,
    MigrationBackend, MigrationEngine, MigrationFlags, MigrationId, MigrationIr,
    MysqlBackend, RowSet, SqlDialect, CURRENT_IR_VERSION,
};

use node_realworld::{allowlist, ensure_mysql, lock_env, EnvGuard, LOCALHOST};

const DEFAULT_DSN: &str = "mysql://root:zeroship@127.0.0.1:3307/zeroship_e2e";
const OWNER: &str = "app_mysql_e2e";

#[derive(Debug, Clone)]
struct MysqlDsn {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

impl MysqlDsn {
    fn json(&self) -> String {
        json!({
            "host": self.host,
            "port": self.port,
            "user": self.user,
            "password": self.password,
            "database": self.database,
        })
        .to_string()
    }
}

#[derive(Debug, Clone)]
struct LiveMysql {
    dsn: MysqlDsn,
    source: String,
}

fn configured_dsn() -> String {
    std::env::var("MYSQL_JS_DRIVER_E2E_DSN").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

fn parse_mysql_dsn(raw: &str) -> MysqlDsn {
    let rest = raw
        .strip_prefix("mysql://")
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must start with mysql://, got {raw:?}"));
    let (auth_host, database) = rest
        .split_once('/')
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must include a database, got {raw:?}"));
    let (auth, host_port) = auth_host
        .rsplit_once('@')
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must include user info, got {raw:?}"));
    let (user, password) = auth
        .split_once(':')
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must include user:password, got {raw:?}"));
    let (host, port) = host_port.rsplit_once(':').unwrap_or((host_port, "3306"));
    MysqlDsn {
        host: host.to_string(),
        port: port
            .parse()
            .unwrap_or_else(|_| panic!("MYSQL_JS_DRIVER_E2E_DSN has invalid port {port:?}")),
        user: user.to_string(),
        password: password.to_string(),
        database: database.to_string(),
    }
}

fn live_mysql_or_skip() -> Option<LiveMysql> {
    let raw = configured_dsn();
    let dsn = parse_mysql_dsn(&raw);
    if raw == DEFAULT_DSN {
        match std::panic::catch_unwind(ensure_mysql) {
            Ok(server) => {
                println!(
                    "mysql_jsdriver_e2e connected to {}:{} via {}",
                    server.host, server.port, server.source
                );
                Some(LiveMysql {
                    dsn,
                    source: server.source,
                })
            }
            Err(_) => {
                eprintln!(
                    "SKIP mysql_jsdriver_e2e: default MySQL {LOCALHOST}:3307 unreachable and ensure_mysql() could not start it"
                );
                None
            }
        }
    } else if node_realworld::tcp_reachable(&dsn.host, dsn.port) {
        println!(
            "mysql_jsdriver_e2e connected to {}:{} via MYSQL_JS_DRIVER_E2E_DSN",
            dsn.host, dsn.port
        );
        Some(LiveMysql {
            dsn,
            source: "MYSQL_JS_DRIVER_E2E_DSN".to_string(),
        })
    } else {
        eprintln!(
            "SKIP mysql_jsdriver_e2e: MYSQL_JS_DRIVER_E2E_DSN target {}:{} is unreachable",
            dsn.host, dsn.port
        );
        None
    }
}

fn token(label: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("zse2a_{label}_{}_{}_{}", std::process::id(), nanos, n)
}

fn qi(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

fn meta_table(cfg: &ExecutorConfig, table: &str) -> String {
    format!("{}.{}", qi(&cfg.pg.meta_schema), qi(table))
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut cfg = ExecutorConfig::new(format!("prj_{tok}"), format!("{tok}_app"));
    cfg.pg.meta_schema = format!("{tok}_meta");
    cfg
}

fn backend_for(live: &LiveMysql) -> MysqlBackend {
    MysqlBackend::open_mysql_dsn_json_with_policy(
        live.dsn.json(),
        allowlist(&live.dsn.host, live.dsn.port, 8, 16 * 1024 * 1024),
        Duration::from_secs(45),
    )
    .expect("open live mysql2 JS driver backend")
}

async fn setup(live: &LiveMysql, label: &str) -> (MysqlBackend, ExecutorConfig) {
    let backend = backend_for(live);
    let cfg = cfg_for(&token(label));
    let _ = backend
        .exec(&format!("DROP SCHEMA IF EXISTS {}", qi(&cfg.project_schema)))
        .await;
    let _ = backend
        .exec(&format!("DROP SCHEMA IF EXISTS {}", qi(&cfg.pg.meta_schema)))
        .await;
    backend
        .exec(&format!("CREATE SCHEMA {}", qi(&cfg.project_schema)))
        .await
        .expect("create isolated MySQL project schema");
    println!(
        "mysql_jsdriver_e2e using project schema {} and meta schema {} ({})",
        cfg.project_schema, cfg.pg.meta_schema, live.source
    );
    (backend, cfg)
}

async fn teardown(backend: &MysqlBackend, cfg: &ExecutorConfig) {
    let _ = backend
        .exec(&format!("DROP SCHEMA IF EXISTS {}", qi(&cfg.project_schema)))
        .await;
    let _ = backend
        .exec(&format!("DROP SCHEMA IF EXISTS {}", qi(&cfg.pg.meta_schema)))
        .await;
}

fn fixture_ir() -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_live_fixture",
        "owner_app": OWNER,
        "ops": [{
            "op": "createTable",
            "name": "users",
            "columns": [
                { "name": "id", "type": "bigInt", "nullable": false, "identity": { "always": false } },
                { "name": "email", "type": "string", "nullable": false },
                { "name": "active", "type": "bool", "nullable": false },
                { "name": "profile", "type": "json", "nullable": true }
            ],
            "constraints": [{ "kind": { "kind": "pk", "columns": ["id"] } }],
            "indexes": [{
                "name": "users_email_idx",
                "columns": [{ "kind": "column", "name": "email" }]
            }]
        }],
        "flags": {},
        "depends_on": [],
        "supersedes": [],
        "preconditions": []
    }))
    .expect("fixture IR is valid")
}

fn fixture_migrations(schema: &str) -> (MigrationIr, Vec<Migration>) {
    let ir = fixture_ir();
    let steps = IrAuthor::new(schema, OWNER, SqlDialect::Mysql)
        .lower_steps(&ir, &LiveSchema::default())
        .expect("lower fixture IR to MySQL");
    let migrations = steps
        .into_iter()
        .map(|step| match step {
            PlanStep::Ddl(m) => m,
            other => panic!("fixture should lower to DDL only, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        !migrations.is_empty(),
        "createTable+index fixture should lower to at least one migration"
    );
    for migration in &migrations {
        println!(
            "mysql_jsdriver_e2e lowered MySQL DDL {}:\n{}",
            migration.version.as_str(),
            migration.up
        );
    }
    (ir, migrations)
}

fn migration_versions(migrations: &[Migration]) -> Vec<String> {
    let mut versions = migrations
        .iter()
        .map(|migration| migration.version.as_str().to_string())
        .collect::<Vec<_>>();
    versions.sort();
    versions
}

fn sorted_versions(mut versions: Vec<String>) -> Vec<String> {
    versions.sort();
    versions
}

async fn apply_fixture(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> zeroship_migrate::ApplyOutcome {
    MigrationEngine::new()
        .apply_verified(
            migrations,
            &GuardConfig::confined_mysql(cfg.project_schema.clone()),
            None,
            Approval::None,
            backend,
            cfg,
            "mysql-jsdriver-e2e",
        )
        .await
        .expect("live MySQL apply succeeds")
}

async fn query(
    backend: &MysqlBackend,
    sql: &str,
    binds: &[zeroship_migrate::render::step::BindValue],
) -> RowSet {
    backend
        .query_json(sql, binds)
        .await
        .unwrap_or_else(|err| panic!("query failed: {err}: {sql}"))
}

fn rows_len(rows: &RowSet) -> usize {
    rows.rows.len()
}

fn field<'a>(row: &'a serde_json::Map<String, Value>, key: &str) -> &'a Value {
    row.get(key)
        .unwrap_or_else(|| panic!("row missing field {key}: {row:?}"))
}

#[test]
fn live_apply_creates_table_and_index_over_mysql2_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let (backend, cfg) = setup(&live, "apply").await;
        let (_ir, migrations) = fixture_migrations(&cfg.project_schema);
        let outcome = apply_fixture(&backend, &cfg, &migrations).await;
        assert_eq!(outcome.applied, migration_versions(&migrations));

        let table_rows = query(
            &backend,
            "SELECT TABLE_NAME FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'users'",
            &[zeroship_migrate::render::step::BindValue::Text(
                cfg.project_schema.clone(),
            )],
        )
        .await;
        let index_rows = query(
            &backend,
            "SELECT INDEX_NAME FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'users' AND INDEX_NAME = 'users_email_idx'",
            &[zeroship_migrate::render::step::BindValue::Text(
                cfg.project_schema.clone(),
            )],
        )
        .await;
        assert_eq!(rows_len(&table_rows), 1, "users table must exist");
        assert_eq!(rows_len(&index_rows), 1, "users_email_idx must exist");
        println!("mysql_jsdriver_e2e apply assertions ran on real information_schema");

        teardown(&backend, &cfg).await;
    });
}

#[test]
fn live_journal_records_completed_and_second_apply_skips() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let (backend, cfg) = setup(&live, "journal").await;
        let (_ir, migrations) = fixture_migrations(&cfg.project_schema);
        apply_fixture(&backend, &cfg, &migrations).await;

        let journal_rows = query(
            &backend,
            &format!(
                "SELECT version, checksum, phase, kind
                   FROM {}
                  WHERE event_kind = 'applied' AND phase = 'completed'
                  ORDER BY version",
                meta_table(&cfg, "schema_migrations")
            ),
            &[],
        )
        .await;
        assert_eq!(rows_len(&journal_rows), migrations.len(), "completed journal rows");
        for migration in &migrations {
            let row = journal_rows
                .rows
                .iter()
                .find(|row| field(row, "version") == &json!(migration.version.as_str()))
                .unwrap_or_else(|| panic!("missing journal row for {}", migration.version.as_str()));
            assert_eq!(field(row, "checksum"), &json!(migration.checksum.as_str()));
            assert_eq!(field(row, "phase"), &json!("completed"));
            assert_eq!(field(row, "kind"), &json!("apply"));
        }
        let applied_entries = backend.applied(&cfg).await.expect("read MySQL net journal state");
        assert_eq!(
            applied_entries
                .iter()
                .map(|entry| entry.version.clone())
                .collect::<Vec<_>>(),
            migration_versions(&migrations),
            "backend.applied must expose completed rows before a second apply"
        );

        let second = apply_fixture(&backend, &cfg, &migrations).await;
        assert!(second.is_noop(), "second apply must be a net-applied no-op: {second:?}");
        assert_eq!(
            sorted_versions(second.skipped),
            migration_versions(&migrations),
            "journal completed row must drive skip"
        );
        let after = query(
            &backend,
            &format!(
                "SELECT version FROM {}
                  WHERE event_kind = 'applied' AND phase = 'completed'",
                meta_table(&cfg, "schema_migrations")
            ),
            &[],
        )
        .await;
        assert_eq!(
            rows_len(&after),
            migrations.len(),
            "second apply must not re-journal/re-execute"
        );
        println!("mysql_jsdriver_e2e journal skip assertions ran");

        teardown(&backend, &cfg).await;
    });
}

#[test]
fn live_two_phase_started_marker_survives_failed_second_fragment_and_ddl_is_visible() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let (backend, cfg) = setup(&live, "phase").await;
        let up = format!(
            "CREATE TABLE {}.{} (id INT NOT NULL);\nCREATE INDEX {} ON {}.{} (id)",
            qi(&cfg.project_schema),
            qi("phase_probe"),
            qi("phase_probe_missing_idx"),
            qi(&cfg.project_schema),
            qi("missing_table"),
        );
        let mut migration = Migration {
            version: MigrationId::generate(),
            name: "phase_started_then_fail".to_string(),
            up,
            down: None,
            checksum: Checksum::of(&ChecksumInput {
                up: "",
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: "",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            flags: MigrationFlags::default(),
            owner_app: OWNER.to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        };
        migration.recompute_checksum();

        let err = MigrationEngine::new()
            .apply_verified(
                std::slice::from_ref(&migration),
                &GuardConfig::confined_mysql(cfg.project_schema.clone()),
                None,
                Approval::None,
                &backend,
                &cfg,
                "mysql-jsdriver-e2e",
            )
            .await
            .expect_err("second fragment should fail");
        assert!(
            matches!(err, EngineError::Apply(ApplyError::MigrationFailed { .. })),
            "unexpected error: {err:?}"
        );

        let inflight = query(
            &backend,
            &format!(
                "SELECT version, checksum FROM {} WHERE version = ?",
                meta_table(&cfg, "schema_migrations_inflight")
            ),
            &[zeroship_migrate::render::step::BindValue::Text(
                migration.version.as_str().to_string(),
            )],
        )
        .await;
        let completed = query(
            &backend,
            &format!(
                "SELECT version FROM {}
                  WHERE event_kind = 'applied' AND version = ? AND phase = 'completed'",
                meta_table(&cfg, "schema_migrations")
            ),
            &[zeroship_migrate::render::step::BindValue::Text(
                migration.version.as_str().to_string(),
            )],
        )
        .await;
        let table = query(
            &backend,
            "SELECT TABLE_NAME FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'phase_probe'",
            &[zeroship_migrate::render::step::BindValue::Text(
                cfg.project_schema.clone(),
            )],
        )
        .await;
        assert_eq!(rows_len(&inflight), 1, "started marker must be visible");
        assert_eq!(rows_len(&completed), 0, "completed marker must not be written");
        assert_eq!(rows_len(&table), 1, "first DDL fragment auto-committed before journal completion");
        println!("mysql_jsdriver_e2e two-phase failure assertions ran");

        teardown(&backend, &cfg).await;
    });
}

#[test]
fn live_snapshot_roundtrips_and_redeploy_is_noop() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let (backend, cfg) = setup(&live, "snapshot").await;
        let (ir, migrations) = fixture_migrations(&cfg.project_schema);
        apply_fixture(&backend, &cfg, &migrations).await;

        let live_snapshot = backend
            .snapshot_schema(&cfg)
            .await
            .expect("snapshot live MySQL schema");
        let expected = fold_ops(&ir.ops, SqlDialect::Mysql, &cfg.project_schema)
            .expect("fold fixture IR to expected MySQL snapshot");
        let drift = diff_snapshots(&expected, &live_snapshot);
        assert!(drift.is_clean(), "live MySQL snapshot should match folded IR: {drift:?}");

        let users = live_snapshot.tables.get("users").expect("users table snapshot");
        assert!(
            users.indexes.iter().any(|idx| idx.name == "users_email_idx"),
            "users_email_idx missing from snapshot: {:?}",
            users.indexes
        );
        let types = users
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect::<Vec<_>>();
        assert!(types.contains(&("active", "boolean", false)), "types: {types:?}");
        assert!(
            types.contains(&("created_at", "timestamp with time zone", false)),
            "types: {types:?}"
        );
        assert!(types.contains(&("profile", "jsonb", true)), "types: {types:?}");

        let redeploy = apply_fixture(&backend, &cfg, &migrations).await;
        assert!(redeploy.is_noop(), "same migration set should redeploy as skip: {redeploy:?}");
        assert_eq!(sorted_versions(redeploy.skipped), migration_versions(&migrations));
        println!("mysql_jsdriver_e2e snapshot + redeploy assertions ran");

        teardown(&backend, &cfg).await;
    });
}
