#![allow(unsafe_code)]

#[path = "../../runtime/tests/support/node_realworld.rs"]
mod node_realworld;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zeroship_migrate::model::probe::{ExpectColumn, GuardDir, GuardProbe};
use zeroship_migrate::render::existence_probe::{decide, GuardVerdict};
use zeroship_migrate::render::step::{BindValue, PlanStep};
use zeroship_migrate::{
    deprovision_mysql_migrator_account, diff_snapshots, fold_ops,
    mysql_migration_lock_name, provision_mysql_migrator_account,
    provision_mysql_migrator_account_with_password, ApplyError, Approval,
    ApprovalScope, Checksum, ChecksumInput, EngineError, ExecutorConfig,
    GuardConfig, IrAuthor, JsDriverError, LiveSchema, LockMode, Migration,
    MigrationBackend, MigrationEngine, MigrationFlags, MigrationId, MigrationIr,
    MysqlBackend, MysqlGuardedFragment, MysqlMigratorAccount, Phase, RowSet,
    SqlDialect, CURRENT_IR_VERSION,
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

    fn json_with_ssl_ca(&self, ca: impl Into<String>) -> String {
        json!({
            "host": self.host,
            "port": self.port,
            "user": self.user,
            "password": self.password,
            "database": self.database,
            "ssl": {
                "ca": ca.into(),
                "rejectUnauthorized": true
            }
        })
        .to_string()
    }

    fn json_with_ssl_without_ca(&self) -> String {
        json!({
            "host": self.host,
            "port": self.port,
            "user": self.user,
            "password": self.password,
            "database": self.database,
            "ssl": {
                "rejectUnauthorized": true
            }
        })
        .to_string()
    }

    fn for_migrator(&self, cfg: &ExecutorConfig, account: &MysqlMigratorAccount) -> Self {
        Self {
            host: self.host.clone(),
            port: self.port,
            user: account.user.clone(),
            password: account.password.clone(),
            database: cfg.project_schema.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct LiveMysql {
    dsn: MysqlDsn,
    source: String,
}

struct NativeRootsGuard {
    path: PathBuf,
    prev_file: Option<std::ffi::OsString>,
    prev_dir: Option<std::ffi::OsString>,
}

impl NativeRootsGuard {
    fn set(ca_pem: &str, label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship-migrate-native-roots-{}-{label}.pem",
            std::process::id()
        ));
        std::fs::write(&path, ca_pem.as_bytes()).expect("write temporary native roots file");
        let prev_file = std::env::var_os("SSL_CERT_FILE");
        let prev_dir = std::env::var_os("SSL_CERT_DIR");
        unsafe {
            std::env::set_var("SSL_CERT_FILE", &path);
            std::env::remove_var("SSL_CERT_DIR");
        }
        Self {
            path,
            prev_file,
            prev_dir,
        }
    }
}

impl Drop for NativeRootsGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_file {
                Some(value) => std::env::set_var("SSL_CERT_FILE", value),
                None => std::env::remove_var("SSL_CERT_FILE"),
            }
            match &self.prev_dir {
                Some(value) => std::env::set_var("SSL_CERT_DIR", value),
                None => std::env::remove_var("SSL_CERT_DIR"),
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
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
    let require = std::env::var("MIGRATE_REQUIRE_MYSQL").is_ok_and(|v| v == "1");
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
                let message = format!(
                    "default MySQL {LOCALHOST}:3307 unreachable and ensure_mysql() could not start it"
                );
                if require {
                    panic!("MIGRATE_REQUIRE_MYSQL=1: {message}");
                }
                eprintln!("SKIPPED (NOT RUN) mysql_jsdriver_e2e: {message}");
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
        let message = format!(
            "MYSQL_JS_DRIVER_E2E_DSN target {}:{} is unreachable",
            dsn.host, dsn.port
        );
        if require {
            panic!("MIGRATE_REQUIRE_MYSQL=1: {message}");
        }
        eprintln!("SKIPPED (NOT RUN) mysql_jsdriver_e2e: {message}");
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

fn backend_for_result(live: &LiveMysql) -> Result<MysqlBackend, JsDriverError> {
    MysqlBackend::open_mysql_dsn_json_with_policy(
        live.dsn.json(),
        allowlist(&live.dsn.host, live.dsn.port, 8, 16 * 1024 * 1024),
        Duration::from_secs(45),
    )
}

fn backend_for(live: &LiveMysql) -> MysqlBackend {
    backend_for_result(live).expect("open live mysql2 JS driver backend")
}

fn migrator_backend_for(
    live: &LiveMysql,
    cfg: &ExecutorConfig,
    account: &MysqlMigratorAccount,
) -> MysqlBackend {
    MysqlBackend::open_mysql_dsn_json_with_policy(
        live.dsn.for_migrator(cfg, account).json(),
        allowlist(&live.dsn.host, live.dsn.port, 8, 16 * 1024 * 1024),
        Duration::from_secs(45),
    )
    .unwrap_or_else(|err| {
        panic!(
            "open live mysql2 JS driver backend as {}: {err}",
            account.user
        )
    })
}

struct MysqlSchemaGuard {
    live: LiveMysql,
    cfg: Option<ExecutorConfig>,
}

impl MysqlSchemaGuard {
    fn new(live: LiveMysql, cfg: ExecutorConfig) -> Self {
        Self {
            live,
            cfg: Some(cfg),
        }
    }

    fn cleanup_now(&mut self) -> Result<(), String> {
        let Some(cfg) = self.cfg.clone() else {
            return Ok(());
        };
        compio::runtime::Runtime::new()
            .map_err(|err| format!("create cleanup runtime failed: {err}"))?
            .block_on(async {
                let backend = backend_for_result(&self.live)
                    .map_err(|err| format!("open cleanup backend failed: {err}"))?;
                teardown(&backend, &cfg).await
            })?;
        self.cfg = None;
        Ok(())
    }
}

impl Drop for MysqlSchemaGuard {
    fn drop(&mut self) {
        if let Err(err) = self.cleanup_now() {
            eprintln!(
                "FAILED mysql_jsdriver_e2e cleanup for panic-safe schema guard: {err}"
            );
        }
    }
}

fn run_isolated_mysql(
    live: &LiveMysql,
    label: &str,
    body: impl for<'a> FnOnce(
        &'a MysqlBackend,
        &'a ExecutorConfig,
        &'a MysqlMigratorAccount,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>>,
) {
    let cfg = cfg_for(&token(label));
    let mut cleanup = MysqlSchemaGuard::new(live.clone(), cfg.clone());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (backend, account) = setup(live, &cfg).await;
            body(&backend, &cfg, &account).await;
        });
    }));
    let cleanup_result = cleanup.cleanup_now();
    if let Err(panic) = result {
        if let Err(err) = cleanup_result {
            eprintln!("FAILED mysql_jsdriver_e2e cleanup after panic: {err}");
        }
        std::panic::resume_unwind(panic);
    }
    cleanup_result.expect("drop isolated MySQL e2e schemas");
}

async fn setup(live: &LiveMysql, cfg: &ExecutorConfig) -> (MysqlBackend, MysqlMigratorAccount) {
    let backend = backend_for(live);
    let _ = deprovision_mysql_migrator_account(&backend, cfg).await;
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
    let account = provision_mysql_migrator_account(&backend, cfg)
        .await
        .expect("provision least-priv MySQL migrator account");
    prewarm_mysql_migrator_auth_cache(live, cfg, &account);
    println!(
        "mysql_jsdriver_e2e using project schema {} and meta schema {} as {}@{} ({})",
        cfg.project_schema, cfg.pg.meta_schema, account.user, account.host, live.source
    );
    drop(backend);
    (migrator_backend_for(live, cfg, &account), account)
}

async fn teardown(backend: &MysqlBackend, cfg: &ExecutorConfig) -> Result<(), String> {
    backend
        .exec(&format!("DROP SCHEMA IF EXISTS {}", qi(&cfg.project_schema)))
        .await
        .map_err(|err| format!("drop project schema {} failed: {err}", cfg.project_schema))?;
    backend
        .exec(&format!("DROP SCHEMA IF EXISTS {}", qi(&cfg.pg.meta_schema)))
        .await
        .map_err(|err| format!("drop meta schema {} failed: {err}", cfg.pg.meta_schema))?;
    deprovision_mysql_migrator_account(backend, cfg)
        .await
        .map_err(|err| {
            format!(
                "drop MySQL migrator account for {} failed: {err}",
                cfg.project_id
            )
        })?;
    Ok(())
}

fn prewarm_mysql_migrator_auth_cache(
    live: &LiveMysql,
    cfg: &ExecutorConfig,
    account: &MysqlMigratorAccount,
) {
    assert_eq!(
        (live.dsn.host.as_str(), live.dsn.port),
        ("127.0.0.1", 3307),
        "mysql_jsdriver_e2e hard e2e expects zeroship-runtime-mysql2-e2e on 127.0.0.1:3307"
    );
    let output = Command::new("docker")
        .arg("exec")
        .arg("zeroship-runtime-mysql2-e2e")
        .arg("mysql")
        .arg("--protocol=TCP")
        .arg("-h127.0.0.1")
        .arg("-P3306")
        .arg(format!("-u{}", account.user))
        .arg(format!("--password={}", account.password))
        .arg("-D")
        .arg(&cfg.project_schema)
        .arg("--batch")
        .arg("--skip-column-names")
        .arg("-e")
        .arg("SELECT CURRENT_USER()")
        .output()
        .expect("run mysql auth-cache prewarm in e2e container");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{} auth-cache prewarm failed: {}\nstdout:\n{}\nstderr:\n{}",
        account.user,
        output.status,
        stdout,
        stderr
    );
    let current_user = stdout.trim();
    assert!(
        current_user.starts_with(&format!("{}@", account.user)),
        "prewarm must authenticate as {}, got {current_user:?}",
        account.user
    );
    println!(
        "mysql_jsdriver_e2e prewarmed caching_sha2 auth cache as {current_user}"
    );
}

fn mysql_server_ca_pem(live: &LiveMysql) -> String {
    assert_eq!(
        (live.dsn.host.as_str(), live.dsn.port),
        ("127.0.0.1", 3307),
        "mysql_jsdriver_e2e TLS cold-cache test reads /var/lib/mysql/ca.pem from \
         zeroship-runtime-mysql2-e2e and must target 127.0.0.1:3307"
    );
    let output = Command::new("docker")
        .arg("exec")
        .arg("zeroship-runtime-mysql2-e2e")
        .arg("sh")
        .arg("-lc")
        .arg("cat /var/lib/mysql/ca.pem")
        .output()
        .expect("read MySQL server CA from e2e container");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "reading MySQL TLS CA from zeroship-runtime-mysql2-e2e failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );
    let ca = stdout.trim().to_string();
    assert!(
        ca.contains("-----BEGIN CERTIFICATE-----")
            && ca.contains("-----END CERTIFICATE-----"),
        "MySQL container CA is not PEM: {ca:?}"
    );
    ca
}

fn invalid_ca_pem() -> &'static str {
    "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
}

fn tls_migrator_backend_for(
    live: &LiveMysql,
    cfg: &ExecutorConfig,
    account: &MysqlMigratorAccount,
    ca_pem: impl Into<String>,
) -> MysqlBackend {
    tls_migrator_backend_result_for(live, cfg, account, ca_pem)
    .unwrap_or_else(|err| {
        panic!(
            "open live TLS mysql2 JS driver backend as {}: {err}",
            account.user
        )
    })
}

fn tls_migrator_backend_result_for(
    live: &LiveMysql,
    cfg: &ExecutorConfig,
    account: &MysqlMigratorAccount,
    ca_pem: impl Into<String>,
) -> Result<MysqlBackend, JsDriverError> {
    MysqlBackend::open_mysql_dsn_json_with_policy(
        live.dsn
            .for_migrator(cfg, account)
            .json_with_ssl_ca(ca_pem),
        allowlist(&live.dsn.host, live.dsn.port, 8, 16 * 1024 * 1024),
        Duration::from_secs(45),
    )
}

fn tls_migrator_backend_without_ca_result_for(
    live: &LiveMysql,
    cfg: &ExecutorConfig,
    account: &MysqlMigratorAccount,
) -> Result<MysqlBackend, JsDriverError> {
    MysqlBackend::open_mysql_dsn_json_with_policy(
        live.dsn.for_migrator(cfg, account).json_with_ssl_without_ca(),
        allowlist(&live.dsn.host, live.dsn.port, 8, 16 * 1024 * 1024),
        Duration::from_secs(45),
    )
}

fn fixture_ir() -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_live_fixture",
        "owner_app": OWNER,
        "ops": [{
            "op": "createTable",
            "name": "users",
            "existenceGuard": "ifNotExists",
            "columns": [
                { "name": "id", "type": "bigInt", "nullable": false, "identity": { "always": false } },
                { "name": "email", "type": "string", "nullable": false },
                { "name": "active", "type": "boolean", "nullable": false },
                { "name": "profile", "type": "json", "nullable": true }
            ],
            "primaryKey": ["id"],
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

fn partition_collapse_ir() -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_partition_collapse",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "events",
                "existenceGuard": "ifNotExists",
                "columns": [
                    { "name": "bucket", "type": "int", "nullable": false },
                    { "name": "payload", "type": "text", "nullable": false }
                ],
                "primaryKey": null,
                "partitionBy": {
                    "kind": "range",
                    "columns": ["bucket"],
                    "collapse": true
                }
            },
            {
                "op": "createPartition",
                "name": "events_0",
                "of": "events",
                "bounds": {
                    "kind": "range",
                    "from": [{ "kind": "int", "value": 0 }],
                    "to": [{ "kind": "int", "value": 100 }]
                }
            },
            {
                "op": "createPartition",
                "name": "events_default",
                "of": "events",
                "bounds": { "kind": "default" }
            }
        ],
        "flags": {},
        "depends_on": [],
        "supersedes": [],
        "preconditions": []
    }))
    .expect("partition collapse IR is valid")
}

fn partition_test_ir(name: &str, ops: Vec<Value>) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": ops,
        "flags": {},
        "depends_on": [],
        "supersedes": [],
        "preconditions": []
    }))
    .expect("partition test IR is valid")
}

fn partition_parent_json() -> Value {
    json!({
        "op": "createTable",
        "name": "events",
        "columns": [
            { "name": "bucket", "type": "int", "nullable": false },
            { "name": "payload", "type": "text", "nullable": false }
        ],
        "primaryKey": null,
        "partitionBy": {
            "kind": "range",
            "columns": ["bucket"],
            "collapse": true
        },
        "existenceGuard": "ifNotExists"
    })
}

fn range_partition_json(name: &str, from: i64, to: i64) -> Value {
    json!({
        "op": "createPartition",
        "name": name,
        "of": "events",
        "bounds": {
            "kind": "range",
            "from": [{ "kind": "int", "value": from }],
            "to": [{ "kind": "int", "value": to }]
        }
    })
}

fn default_partition_json() -> Value {
    json!({
        "op": "createPartition",
        "name": "events_default",
        "of": "events",
        "bounds": { "kind": "default" }
    })
}

fn drop_partition_json(name: &str) -> Value {
    json!({
        "op": "dropPartition",
        "parent": "events",
        "name": name
    })
}

fn insert_events_json(rows: &[(i64, &str)]) -> Value {
    let rows = rows
        .iter()
        .map(|(bucket, payload)| json!([bucket, payload]))
        .collect::<Vec<_>>();
    json!({
        "op": "insert",
        "table": "events",
        "columns": ["bucket", "payload"],
        "rows": rows
    })
}

fn drop_events_table_json() -> Value {
    json!({
        "op": "dropTable",
        "table": "events",
        "existenceGuard": "ifExists"
    })
}

fn live_from_partition_fold(schema: &str, ops: &[zeroship_migrate::Op]) -> LiveSchema {
    let snap = fold_ops(ops, SqlDialect::Mysql, schema).expect("fold partition ops");
    let mut live = LiveSchema::from_tables(snap.tables.keys().cloned().collect());
    live.table_snapshots = snap.tables;
    live.partitions = snap.partitions;
    live
}

fn lower_mysql_steps_with_live(
    schema: &str,
    ir: &MigrationIr,
    live: &LiveSchema,
    label: &str,
) -> Vec<PlanStep> {
    IrAuthor::new(schema, OWNER, SqlDialect::Mysql)
        .lower_steps(ir, live)
        .unwrap_or_else(|err| panic!("lower {label} IR to MySQL steps: {err}"))
}

async fn apply_mysql_steps(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    steps: &[PlanStep],
    approval: Approval,
) -> Result<(), String> {
    for step in steps {
        match step {
            PlanStep::Ddl(migration) => {
                MigrationEngine::new()
                    .apply_verified(
                        std::slice::from_ref(migration),
                        &GuardConfig::confined_mysql(cfg.project_schema.clone()),
                        None,
                        approval,
                        backend,
                        cfg,
                        OWNER,
                    )
                    .await
                    .map_err(|err| format!("{err:?}"))?;
            }
            PlanStep::Dml { .. } | PlanStep::Backfill(_) | PlanStep::OnlineRename(_) => {
                MigrationEngine::new()
                    .apply_plan(
                        std::slice::from_ref(step),
                        approval,
                        backend,
                        cfg,
                        OWNER,
                        LockMode::Acquire,
                    )
                    .await
                    .map_err(|err| format!("{err:?}"))?;
            }
        }
    }
    Ok(())
}

async fn mysql_event_rows(backend: &MysqlBackend) -> Vec<(i64, String)> {
    let rows = query(
        backend,
        "SELECT bucket, payload FROM events ORDER BY bucket",
        &[],
    )
    .await;
    rows.rows
        .iter()
        .map(|row| {
            (
                value_as_i64(field(row, "bucket")),
                value_as_string(row.get("payload")),
            )
        })
        .collect()
}

fn fixture_migrations(schema: &str) -> (MigrationIr, Vec<Migration>) {
    let ir = fixture_ir();
    let migrations = lower_mysql_migrations(schema, &ir, "fixture");
    (ir, migrations)
}

fn lower_mysql_migrations(schema: &str, ir: &MigrationIr, label: &str) -> Vec<Migration> {
    let steps = IrAuthor::new(schema, OWNER, SqlDialect::Mysql)
        .lower_steps(&ir, &LiveSchema::default())
        .unwrap_or_else(|err| panic!("lower {label} IR to MySQL: {err}"));
    let migrations = steps
        .into_iter()
        .map(|step| match step {
            PlanStep::Ddl(m) => m,
            other => panic!("{label} should lower to DDL only, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        !migrations.is_empty(),
        "{label} should lower to at least one migration"
    );
    for migration in &migrations {
        println!(
            "mysql_jsdriver_e2e lowered MySQL {label} DDL {}:\n{}",
            migration.version.as_str(),
            migration.up
        );
    }
    migrations
}

fn fk_roundtrip_ir() -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_fk_roundtrip",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "accounts",
                "existenceGuard": "ifNotExists",
                "columns": [
                    { "name": "id", "type": "int", "nullable": false, "identity": { "always": false } },
                    { "name": "name", "type": "text", "nullable": false }
                ],
                "primaryKey": ["id"]
            },
            {
                "op": "createTable",
                "name": "orders",
                "existenceGuard": "ifNotExists",
                "columns": [
                    { "name": "id", "type": "int", "nullable": false, "identity": { "always": false } },
                    { "name": "account_id", "type": "int", "nullable": true },
                    { "name": "code", "type": "text", "nullable": false }
                ],
                "primaryKey": ["id"],
                "constraints": [
                    { "name": "orders_account_fkey", "kind": {
                        "kind": "fk",
                        "columns": ["account_id"],
                        "referencesTable": "accounts",
                        "referencesColumns": ["id"],
                        "onDelete": "setNull",
                        "onUpdate": "cascade"
                    } }
                ]
            }
        ],
        "flags": {},
        "depends_on": [],
        "supersedes": [],
        "preconditions": []
    }))
    .expect("FK roundtrip IR is valid")
}

fn fk_default_action_ir(on_delete: Option<&str>, on_update: Option<&str>) -> MigrationIr {
    let mut fk_kind = json!({
        "kind": "fk",
        "columns": ["account_id"],
        "referencesTable": "accounts",
        "referencesColumns": ["id"]
    });
    let kind = fk_kind
        .as_object_mut()
        .expect("FK kind fixture is an object");
    if let Some(action) = on_delete {
        kind.insert("onDelete".to_string(), json!(action));
    }
    if let Some(action) = on_update {
        kind.insert("onUpdate".to_string(), json!(action));
    }

    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_fk_default_action",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "accounts",
                "existenceGuard": "ifNotExists",
                "columns": [
                    { "name": "id", "type": "int", "nullable": false, "identity": { "always": false } },
                    { "name": "name", "type": "text", "nullable": false }
                ],
                "primaryKey": ["id"]
            },
            {
                "op": "createTable",
                "name": "orders",
                "existenceGuard": "ifNotExists",
                "columns": [
                    { "name": "id", "type": "int", "nullable": false, "identity": { "always": false } },
                    { "name": "account_id", "type": "int", "nullable": true },
                    { "name": "code", "type": "text", "nullable": false }
                ],
                "primaryKey": ["id"],
                "constraints": [
                    { "name": "orders_account_fkey", "kind": fk_kind }
                ]
            }
        ],
        "flags": {},
        "depends_on": [],
        "supersedes": [],
        "preconditions": []
    }))
    .expect("FK default-action IR is valid")
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

async fn lower_plan_and_apply_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
) -> zeroship_migrate::engine::DeclarativeDeployOutcome {
    let document = zeroship_migrate::model::load::load_ir_document(
        ir,
        OWNER,
        zeroship_migrate::model::validate::Dialect::Mysql,
        reg,
        None,
        None,
    )
    .expect("load gate");
    let author = IrAuthor::new(cfg.project_schema.clone(), OWNER, SqlDialect::Mysql);
    let plan = author
        .lower_plan(&document, &LiveSchema::default())
        .expect("lower the IR plan on MySQL");
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            Approval::None,
            backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .expect("apply the authored DML plan on MySQL")
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

fn field_any<'a>(
    row: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> &'a Value {
    keys.iter()
        .find_map(|key| row.get(*key))
        .unwrap_or_else(|| panic!("row missing any of {keys:?}: {row:?}"))
}

fn value_as_i64(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|v| i64::try_from(v).ok()))
            .unwrap_or_else(|| panic!("numeric value is not an i64: {value}")),
        Value::String(s) => s
            .parse::<i64>()
            .unwrap_or_else(|err| panic!("string value is not an i64 ({s:?}): {err}")),
        other => panic!("value is not an i64-compatible scalar: {other}"),
    }
}

fn value_as_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

async fn mysql_connection_id(backend: &MysqlBackend) -> i64 {
    let rows = query(backend, "SELECT CONNECTION_ID() AS connection_id", &[]).await;
    value_as_i64(field(&rows.rows[0], "connection_id"))
}

fn manual_migration(name: &str, up: String) -> Migration {
    let mut migration = Migration {
        version: MigrationId::generate(),
        name: name.to_string(),
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
    migration
}

fn guarded_create_table_migration(name: &str, schema: &str, table: &str) -> Migration {
    let mut migration = manual_migration(
        name,
        format!(
            "CREATE TABLE {}.{} (id INT NOT NULL)",
            qi(schema),
            qi(table)
        ),
    );
    migration.existence_guard = Some(GuardProbe::Table {
        schema: schema.to_string(),
        table: table.to_string(),
        direction: GuardDir::IfNotExists,
        expect_columns: vec![ExpectColumn {
            name: "id".to_string(),
            data_type: "integer".to_string(),
            nullable: false,
        }],
    });
    migration.recompute_checksum();
    migration
}

fn crash_recovery_migration(
    cfg: &ExecutorConfig,
) -> (Migration, Vec<MysqlGuardedFragment>) {
    let table_sql = format!(
        "CREATE TABLE {}.{} (id INT NOT NULL, email VARCHAR(191) NOT NULL)",
        qi(&cfg.project_schema),
        qi("crash_users")
    );
    let index_sql = format!(
        "CREATE INDEX {} ON {}.{} (email)",
        qi("crash_users_email_idx"),
        qi(&cfg.project_schema),
        qi("crash_users")
    );
    let sleep_sql = "SELECT SLEEP(8)".to_string();
    let migration = manual_migration(
        "crash_recovery_create_table_then_index",
        format!("{table_sql};\n{sleep_sql};\n{index_sql}"),
    );
    let fragments = vec![
        MysqlGuardedFragment {
            sql: table_sql,
            existence_guard: GuardProbe::Table {
                schema: cfg.project_schema.clone(),
                table: "crash_users".to_string(),
                direction: GuardDir::IfNotExists,
                expect_columns: vec![
                    ExpectColumn {
                        name: "id".to_string(),
                        data_type: "integer".to_string(),
                        nullable: false,
                    },
                    ExpectColumn {
                        name: "email".to_string(),
                        data_type: "text".to_string(),
                        nullable: false,
                    },
                ],
            },
        },
        MysqlGuardedFragment {
            sql: sleep_sql,
            existence_guard: GuardProbe::Index {
                schema: cfg.project_schema.clone(),
                table: "crash_users".to_string(),
                name: "crash_users_email_idx".to_string(),
                direction: GuardDir::IfNotExists,
                expect: Some((false, vec!["email".to_string()])),
            },
        },
        MysqlGuardedFragment {
            sql: index_sql,
            existence_guard: GuardProbe::Index {
                schema: cfg.project_schema.clone(),
                table: "crash_users".to_string(),
                name: "crash_users_email_idx".to_string(),
                direction: GuardDir::IfNotExists,
                expect: Some((false, vec!["email".to_string()])),
            },
        },
    ];
    (migration, fragments)
}

fn crash_recovery_faildrift_migration(
    cfg: &ExecutorConfig,
) -> (Migration, Vec<MysqlGuardedFragment>) {
    let table_sql = format!(
        "CREATE TABLE {}.{} (id INT NOT NULL, email VARCHAR(191) NOT NULL)",
        qi(&cfg.project_schema),
        qi("drift_users")
    );
    let sleep_sql = "SELECT SLEEP(8)".to_string();
    let migration = manual_migration(
        "crash_recovery_faildrift_table_then_probe",
        format!("{table_sql};\n{sleep_sql}"),
    );
    let fragments = vec![
        MysqlGuardedFragment {
            sql: table_sql,
            existence_guard: GuardProbe::Table {
                schema: cfg.project_schema.clone(),
                table: "drift_users".to_string(),
                direction: GuardDir::IfNotExists,
                expect_columns: vec![
                    ExpectColumn {
                        name: "id".to_string(),
                        data_type: "integer".to_string(),
                        nullable: false,
                    },
                    ExpectColumn {
                        name: "email".to_string(),
                        data_type: "text".to_string(),
                        nullable: false,
                    },
                ],
            },
        },
        MysqlGuardedFragment {
            sql: sleep_sql,
            existence_guard: GuardProbe::Index {
                schema: cfg.project_schema.clone(),
                table: "drift_users".to_string(),
                name: "drift_users_email_idx".to_string(),
                direction: GuardDir::IfNotExists,
                expect: Some((false, vec!["email".to_string()])),
            },
        },
    ];
    (migration, fragments)
}

async fn apply_one_result(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    migration: &Migration,
    actor: &str,
) -> Result<zeroship_migrate::ApplyOutcome, EngineError> {
    MigrationEngine::new()
        .apply_verified(
            std::slice::from_ref(migration),
            &GuardConfig::confined_mysql(cfg.project_schema.clone()),
            None,
            Approval::None,
            backend,
            cfg,
            actor,
        )
        .await
}

async fn table_exists(backend: &MysqlBackend, schema: &str, table: &str) -> bool {
    let rows = query(
        backend,
        "SELECT TABLE_NAME FROM information_schema.TABLES
          WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        &[
            zeroship_migrate::render::step::BindValue::Text(schema.to_string()),
            zeroship_migrate::render::step::BindValue::Text(table.to_string()),
        ],
    )
    .await;
    rows_len(&rows) == 1
}

async fn index_exists(backend: &MysqlBackend, schema: &str, table: &str, index: &str) -> bool {
    let rows = query(
        backend,
        "SELECT INDEX_NAME FROM information_schema.STATISTICS
          WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ?",
        &[
            BindValue::Text(schema.to_string()),
            BindValue::Text(table.to_string()),
            BindValue::Text(index.to_string()),
        ],
    )
    .await;
    rows_len(&rows) == 1
}

async fn wait_for_project_lock_released(backend: &MysqlBackend, cfg: &ExecutorConfig) {
    let lock_name = mysql_migration_lock_name(&cfg.project_id);
    let started = Instant::now();
    loop {
        let rows = query(
            backend,
            "SELECT GET_LOCK(?, 0) AS acquired",
            &[BindValue::Text(lock_name.clone())],
        )
        .await;
        let acquired = field(&rows.rows[0], "acquired");
        if acquired == &json!(1) || acquired == &json!("1") {
            backend
                .exec_with_binds(
                    "SELECT RELEASE_LOCK(?)",
                    &[BindValue::Text(lock_name.clone())],
                )
                .await
                .expect("release lock probe");
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for injected-crash MySQL session lock to vanish"
        );
        compio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_sleep_fragment_killer(
    live: LiveMysql,
    connection_id: i64,
    sleep_marker: &'static str,
) -> JoinHandle<()> {
    thread::spawn(move || {
        compio::runtime::Runtime::new().unwrap().block_on(async move {
            let admin = backend_for(&live);
            let started = Instant::now();
            loop {
                let rows = query(
                    &admin,
                    "SELECT COMMAND, STATE, INFO FROM information_schema.PROCESSLIST WHERE ID = ?",
                    &[BindValue::Int(connection_id)],
                )
                .await;
                if let Some(row) = rows.rows.first() {
                    let command = value_as_string(row.get("COMMAND"));
                    let state = value_as_string(row.get("STATE"));
                    let info = value_as_string(row.get("INFO"));
                    let haystack = format!("{command}\n{state}\n{info}").to_ascii_uppercase();
                    if haystack.contains(sleep_marker) || haystack.contains("USER SLEEP") {
                        admin
                            .exec(&format!("KILL CONNECTION {connection_id}"))
                            .await
                            .unwrap_or_else(|err| {
                                panic!("kill migration connection {connection_id} failed: {err}")
                            });
                        println!(
                            "mysql_jsdriver_e2e killed migration connection {connection_id} during crash-recovery sleep fragment"
                        );
                        return;
                    }
                }
                assert!(
                    started.elapsed() < Duration::from_secs(15),
                    "timed out waiting for migration connection {connection_id} to enter sleep fragment"
                );
                compio::time::sleep(Duration::from_millis(25)).await;
            }
        });
    })
}

async fn wait_for_table_or_child_exit(
    backend: &MysqlBackend,
    schema: &str,
    table: &str,
    timeout: Duration,
    child: &mut Child,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if table_exists(backend, schema, table).await {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|err| format!("poll helper child failed: {err}"))?
        {
            return Err(format!(
                "helper child exited before {schema}.{table} became visible: {status}"
            ));
        }
        if start.elapsed() >= timeout {
            return Err(format!(
                "timed out waiting for {schema}.{table} while helper child was still running"
            ));
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn spawn_lock_first_apply_helper(
    live: &LiveMysql,
    cfg: &ExecutorConfig,
    account: &MysqlMigratorAccount,
) -> Child {
    let exe = std::env::current_exe().expect("current test binary path");
    Command::new(exe)
        .arg("--exact")
        .arg("mysql_advisory_lock_first_apply_helper")
        .arg("--ignored")
        .arg("--nocapture")
        .env("ZS_MYSQL_LOCK_HELPER", "1")
        .env("ZS_MYSQL_LOCK_HELPER_HOST", &live.dsn.host)
        .env("ZS_MYSQL_LOCK_HELPER_PORT", live.dsn.port.to_string())
        .env("ZS_MYSQL_LOCK_HELPER_DATABASE", &live.dsn.database)
        .env("ZS_MYSQL_LOCK_HELPER_PROJECT_ID", &cfg.project_id)
        .env("ZS_MYSQL_LOCK_HELPER_PROJECT_SCHEMA", &cfg.project_schema)
        .env("ZS_MYSQL_LOCK_HELPER_META_SCHEMA", &cfg.pg.meta_schema)
        .env("ZS_MYSQL_LOCK_HELPER_USER", &account.user)
        .env("ZS_MYSQL_LOCK_HELPER_PASSWORD", &account.password)
        .env("MIGRATE_REQUIRE_MYSQL", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MySQL lock helper test process")
}

fn wait_for_helper_success(child: Child, context: &str) {
    let output = child
        .wait_with_output()
        .expect("wait for MySQL lock helper child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        panic!(
            "MySQL lock helper failed during {context}: {}\nstdout:\n{}\nstderr:\n{}",
            output.status, stdout, stderr
        );
    }
    println!("mysql_jsdriver_e2e lock helper output:\n{stdout}");
    if !stderr.trim().is_empty() {
        eprintln!("mysql_jsdriver_e2e lock helper stderr:\n{stderr}");
    }
}

fn assert_mysql_access_denied(err: JsDriverError, context: &str) {
    match err {
        JsDriverError::Remote { code, sqlstate, message }
            if matches!(code, 1044 | 1045 | 1142 | 1227 | 1410) =>
        {
            println!(
                "mysql_jsdriver_e2e confinement denied {context}: code={code} sqlstate={sqlstate} message={message}"
            );
        }
        other => panic!("expected MySQL access-denied error for {context}, got {other:?}"),
    }
}

fn assert_mysql_access_denied_1142(err: JsDriverError, context: &str) {
    match err {
        JsDriverError::Remote {
            code: 1142,
            sqlstate,
            message,
        } => {
            println!(
                "mysql_jsdriver_e2e confinement denied {context}: code=1142 sqlstate={sqlstate} message={message}"
            );
        }
        other => panic!("expected MySQL 1142 access-denied error for {context}, got {other:?}"),
    }
}

fn assert_mysql_lock_timeout(err: EngineError, waited: Duration) {
    let EngineError::Apply(ApplyError::Db(db)) = err else {
        panic!("expected lock timeout as EngineError::Apply(Db), got {err:?}");
    };
    let remote = db
        .downcast_ref::<JsDriverError>()
        .unwrap_or_else(|| panic!("lock timeout should downcast to JsDriverError: {db}"));
    assert!(
        matches!(
            remote,
            JsDriverError::Remote {
                code: 1205,
                sqlstate,
                ..
            } if sqlstate == "HY000"
        ),
        "expected synthetic MySQL lock-timeout remote error, got {remote:?}"
    );
    assert!(
        waited >= Duration::from_millis(100),
        "second apply should have actually waited on GET_LOCK, waited {waited:?}"
    );
    println!("mysql_jsdriver_e2e advisory lock contention returned {remote:?} after {waited:?}");
}

#[test]
fn live_teardown_guard_drops_schemas_after_panic() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    let cfg = cfg_for(&token("panic"));
    let mut cleanup = MysqlSchemaGuard::new(live.clone(), cfg.clone());
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {
        eprintln!("mysql_jsdriver_e2e intentional cleanup probe panic captured");
    }));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (backend, _account) = setup(&live, &cfg).await;
            backend
                .exec(&format!(
                    "CREATE TABLE {}.{} (id INT NOT NULL)",
                    qi(&cfg.project_schema),
                    qi("panic_probe")
                ))
                .await
                .expect("create project table for panic-cleanup probe");
            panic!("intentional mysql_jsdriver_e2e cleanup probe panic");
        });
    }));
    std::panic::set_hook(previous_hook);
    assert!(result.is_err(), "cleanup probe must exercise an unwind");
    cleanup
        .cleanup_now()
        .expect("panic-safe cleanup should drop test schemas");

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let backend = backend_for(&live);
        let rows = query(
            &backend,
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA
              WHERE SCHEMA_NAME IN (?, ?)
              ORDER BY SCHEMA_NAME",
            &[
                zeroship_migrate::render::step::BindValue::Text(cfg.project_schema.clone()),
                zeroship_migrate::render::step::BindValue::Text(cfg.pg.meta_schema.clone()),
            ],
        )
        .await;
        assert_eq!(
            rows_len(&rows),
            0,
            "panic-safe cleanup left zse2a schemas behind: {:?}",
            rows.rows
        );
    });
}

#[test]
fn per_project_migrator_accounts_are_isolated() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    let cfg_a = cfg_for(&token("iso_a"));
    let cfg_b = cfg_for(&token("iso_b"));
    let mut cleanup_a = MysqlSchemaGuard::new(live.clone(), cfg_a.clone());
    let mut cleanup_b = MysqlSchemaGuard::new(live.clone(), cfg_b.clone());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (backend_a, account_a) = setup(&live, &cfg_a).await;
            let (backend_b, account_b) = setup(&live, &cfg_b).await;

            assert_ne!(
                account_a.user, account_b.user,
                "distinct projects must not share one MySQL migrator account"
            );

            let admin = backend_for(&live);
            let account_rows = query(
                &admin,
                "SELECT User FROM mysql.user
                  WHERE User IN (?, ?)
                  ORDER BY User",
                &[
                    zeroship_migrate::render::step::BindValue::Text(account_a.user.clone()),
                    zeroship_migrate::render::step::BindValue::Text(account_b.user.clone()),
                ],
            )
            .await;
            assert_eq!(
                rows_len(&account_rows),
                2,
                "both per-project accounts must coexist in mysql.user: {:?}",
                account_rows.rows
            );

            backend_a
                .exec(&format!(
                    "CREATE TABLE {}.{} (id INT NOT NULL)",
                    qi(&cfg_a.project_schema),
                    qi("a_owned_after_b_provision")
                ))
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "{} lost its in-scope grants after {} was provisioned: {err}",
                        account_a.user, account_b.user
                    )
                });
            assert!(
                table_exists(&backend_a, &cfg_a.project_schema, "a_owned_after_b_provision").await,
                "project A account should materialize in-scope DDL"
            );

            let cross_schema = match backend_a
                .exec(&format!(
                    "CREATE TABLE {}.{} (id INT NOT NULL)",
                    qi(&cfg_b.project_schema),
                    qi("a_forbidden_on_b")
                ))
                .await
            {
                Ok(()) => panic!(
                    "{} must not be able to create objects in {}",
                    account_a.user, cfg_b.project_schema
                ),
                Err(err) => err,
            };
            assert_mysql_access_denied_1142(
                cross_schema,
                "project A account DDL on project B schema",
            );

            let reprovisioned_a = provision_mysql_migrator_account(&admin, &cfg_a)
                .await
                .expect("re-provision project A migrator account");
            assert_eq!(
                reprovisioned_a.user, account_a.user,
                "re-provisioning a project must keep the same derived account name"
            );
            prewarm_mysql_migrator_auth_cache(&live, &cfg_a, &reprovisioned_a);
            let backend_a_reprovisioned = migrator_backend_for(&live, &cfg_a, &reprovisioned_a);
            backend_a_reprovisioned
                .exec(&format!(
                    "CREATE TABLE {}.{} (id INT NOT NULL)",
                    qi(&cfg_a.project_schema),
                    qi("a_owned_after_a_reprovision")
                ))
                .await
                .expect("project A should keep its grants after A re-provision");

            backend_b
                .exec(&format!(
                    "CREATE TABLE {}.{} (id INT NOT NULL)",
                    qi(&cfg_b.project_schema),
                    qi("b_owned_after_a_reprovision")
                ))
                .await
                .expect("project B should keep its grants after A re-provision");

            println!(
                "mysql_jsdriver_e2e per-project migrator isolation assertions ran on real :3307: {} and {}",
                account_a.user, account_b.user
            );
        });
    }));
    let cleanup_a_result = cleanup_a.cleanup_now();
    let cleanup_b_result = cleanup_b.cleanup_now();
    if let Err(panic) = result {
        if let Err(err) = cleanup_a_result {
            eprintln!("FAILED mysql_jsdriver_e2e project A isolation cleanup after panic: {err}");
        }
        if let Err(err) = cleanup_b_result {
            eprintln!("FAILED mysql_jsdriver_e2e project B isolation cleanup after panic: {err}");
        }
        std::panic::resume_unwind(panic);
    }
    cleanup_a_result.expect("drop isolated MySQL e2e project A schema/account");
    cleanup_b_result.expect("drop isolated MySQL e2e project B schema/account");
}

#[test]
fn live_apply_creates_table_and_index_over_mysql2_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "apply", |backend, cfg, _account| Box::pin(async move {
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
    }));
}

#[test]
fn live_partition_collapse_applies_as_plain_table_over_mysql2_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "partitioncollapse", |backend, cfg, _account| Box::pin(async move {
        let ir = partition_collapse_ir();
        zeroship_migrate::model::validate::validate_ir_scoped(
            &ir,
            zeroship_migrate::model::validate::Dialect::Mysql,
            &[],
            None,
            &zeroship_migrate::PolicyProfile::platform(),
        )
        .expect("collapse-affirmed partition recording validates on MySQL");
        let steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &ir,
            &LiveSchema::default(),
            "partition_collapse",
        );
        assert_eq!(
            steps.len(),
            2,
            "bounded createPartition lowers to a mirror guard DML step; default child remains no-DDL on MySQL"
        );
        let rendered = steps
            .iter()
            .map(|step| match step {
                PlanStep::Ddl(migration) => migration.up.clone(),
                PlanStep::Dml { template, .. } => template.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("partitionBy collapsed to a plain table"),
            "degraded leg should be visible in MySQL plan output:\n{rendered}"
        );
        assert!(
            rendered.contains("partition collapse populated-default mirror guard"),
            "bounded child create should carry the populated-default mirror guard:\n{rendered}"
        );
        assert!(
            !rendered.contains("PARTITION BY") && !rendered.contains("PARTITION OF"),
            "MySQL collapse must not emit native partition syntax:\n{rendered}"
        );
        assert!(
            !rendered.contains("events_0") && !rendered.contains("events_default"),
            "MySQL collapse must not emit child table DDL:\n{rendered}"
        );

        apply_mysql_steps(backend, cfg, &steps, Approval::None)
            .await
            .expect("apply partition collapse parent + mirror guard");
        backend
            .exec(&format!(
                "INSERT INTO {}.{} (bucket, payload) VALUES (42, 'range'), (250, 'default')",
                qi(&cfg.project_schema),
                qi("events")
            ))
            .await
            .expect("insert rows into collapsed MySQL table");
        let rows = query(
            backend,
            &format!(
                "SELECT bucket, payload FROM {}.{} ORDER BY bucket",
                qi(&cfg.project_schema),
                qi("events")
            ),
            &[],
        )
        .await;
        assert_eq!(rows_len(&rows), 2, "both rows must read from collapsed table");
        assert_eq!(value_as_i64(field(&rows.rows[0], "bucket")), 42);
        assert_eq!(value_as_string(rows.rows[0].get("payload")), "range");
        assert_eq!(value_as_i64(field(&rows.rows[1], "bucket")), 250);
        assert_eq!(value_as_string(rows.rows[1].get("payload")), "default");

        let child_tables = query(
            backend,
            "SELECT TABLE_NAME FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME IN ('events_0', 'events_default')",
            &[BindValue::Text(cfg.project_schema.clone())],
        )
        .await;
        assert_eq!(
            rows_len(&child_tables),
            0,
            "collapse child partitions must be no-DDL on MySQL"
        );
    }));
}

#[test]
fn live_partition_collapse_bounded_child_drop_deletes_bound_rows_over_mysql2_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "partitiondrop", |backend, cfg, _account| Box::pin(async move {
        let ir = partition_test_ir(
            "mysql_partition_bounded_drop",
            vec![
                partition_parent_json(),
                range_partition_json("events_0", 0, 100),
                default_partition_json(),
                insert_events_json(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
                drop_partition_json("events_0"),
            ],
        );
        let steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &ir,
            &LiveSchema::default(),
            "partition_bounded_drop",
        );
        let rendered = format!("{steps:#?}");
        assert!(
            rendered.contains("partition child drop collapsed to DELETE FROM parent")
                && rendered.contains("`bucket` >= 0 AND `bucket` < 100"),
            "bounded child drop must lower to a bounded DELETE on MySQL:\n{rendered}"
        );
        apply_mysql_steps(backend, cfg, &steps, Approval::Approved)
            .await
            .expect("apply bounded child drop on MySQL");
        assert_eq!(
            mysql_event_rows(backend).await,
            vec![
                (150, "default-a".to_string()),
                (250, "default-b".to_string()),
            ]
        );
    }));
}

#[test]
fn live_partition_collapse_default_child_drop_deletes_residual_rows_over_mysql2_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "partitiondefaultdrop", |backend, cfg, _account| Box::pin(async move {
        let ir = partition_test_ir(
            "mysql_partition_default_drop",
            vec![
                partition_parent_json(),
                range_partition_json("events_0", 0, 100),
                default_partition_json(),
                insert_events_json(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
                drop_partition_json("events_default"),
            ],
        );
        let steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &ir,
            &LiveSchema::default(),
            "partition_default_drop",
        );
        let rendered = format!("{steps:#?}");
        assert!(
            rendered.contains("NOT (`bucket` >= 0 AND `bucket` < 100)"),
            "default child drop must lower to residual sibling negation on MySQL:\n{rendered}"
        );
        apply_mysql_steps(backend, cfg, &steps, Approval::Approved)
            .await
            .expect("apply default child drop on MySQL");
        assert_eq!(
            mysql_event_rows(backend).await,
            vec![(42, "range".to_string())]
        );
    }));
}

#[test]
fn live_partition_collapse_mirror_guard_errors_only_for_matching_default_rows_over_mysql2_node_net()
{
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "partitionguarddirty", |backend, cfg, _account| Box::pin(async move {
        let dirty = partition_test_ir(
            "mysql_partition_guard_dirty",
            vec![
                partition_parent_json(),
                default_partition_json(),
                insert_events_json(&[(42, "stray")]),
                range_partition_json("events_0", 0, 100),
            ],
        );
        let dirty_steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &dirty,
            &LiveSchema::default(),
            "partition_guard_dirty",
        );
        let rendered = format!("{dirty_steps:#?}");
        assert!(
            rendered.contains("partition collapse populated-default mirror guard")
                && rendered.contains("JSON_EXTRACT('zeroship_partition_mirror_guard', '$')"),
            "bounded create must carry the MySQL mirror guard:\n{rendered}"
        );
        apply_mysql_steps(backend, cfg, &dirty_steps, Approval::None)
            .await
            .expect_err("matching default rows must trip the MySQL mirror guard");
    }));

    run_isolated_mysql(&live, "partitionguardclean", |backend, cfg, _account| Box::pin(async move {
        let clean = partition_test_ir(
            "mysql_partition_guard_clean",
            vec![
                partition_parent_json(),
                default_partition_json(),
                insert_events_json(&[(250, "default")]),
                range_partition_json("events_0", 0, 100),
            ],
        );
        let clean_steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &clean,
            &LiveSchema::default(),
            "partition_guard_clean",
        );
        apply_mysql_steps(backend, cfg, &clean_steps, Approval::None)
            .await
            .expect("non-matching default rows must not trip the MySQL mirror guard");
        assert_eq!(
            mysql_event_rows(backend).await,
            vec![(250, "default".to_string())]
        );
    }));
}

#[test]
fn live_partition_collapse_child_create_down_round_trip_over_mysql2_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "partitiondown", |backend, cfg, _account| Box::pin(async move {
        let up = partition_test_ir(
            "mysql_partition_up",
            vec![
                partition_parent_json(),
                range_partition_json("events_0", 0, 100),
                default_partition_json(),
                insert_events_json(&[(42, "range"), (150, "default-a"), (250, "default-b")]),
            ],
        );
        let up_steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &up,
            &LiveSchema::default(),
            "partition_up",
        );
        apply_mysql_steps(backend, cfg, &up_steps, Approval::None)
            .await
            .expect("apply MySQL partition up");
        assert_eq!(
            mysql_event_rows(backend).await,
            vec![
                (42, "range".to_string()),
                (150, "default-a".to_string()),
                (250, "default-b".to_string()),
            ]
        );

        let live = live_from_partition_fold(&cfg.project_schema, &up.ops);
        let down_children = partition_test_ir(
            "mysql_partition_down_children",
            vec![drop_partition_json("events_default"), drop_partition_json("events_0")],
        );
        let down_child_steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &down_children,
            &live,
            "partition_down_children",
        );
        apply_mysql_steps(backend, cfg, &down_child_steps, Approval::Approved)
            .await
            .expect("apply MySQL semantic child drops");
        assert!(
            mysql_event_rows(backend).await.is_empty(),
            "child drops should remove all rows before parent drop"
        );

        let down_parent = partition_test_ir("mysql_partition_down_parent", vec![drop_events_table_json()]);
        let down_parent_steps = lower_mysql_steps_with_live(
            &cfg.project_schema,
            &down_parent,
            &LiveSchema::default(),
            "partition_down_parent",
        );
        apply_mysql_steps(backend, cfg, &down_parent_steps, Approval::Approved)
            .await
            .expect("apply MySQL semantic down");
        let tables = query(backend, "SHOW TABLES LIKE 'events'", &[]).await;
        assert_eq!(rows_len(&tables), 0, "events table should be gone after down");
    }));
}

#[test]
fn live_journal_records_completed_and_second_apply_skips() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "journal", |backend, cfg, _account| Box::pin(async move {
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
    }));
}

#[test]
fn in_list_predicates_apply_identically_on_mysql() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "inlist", |backend, cfg, _account| Box::pin(async move {
        backend
            .exec(&format!(
                "CREATE TABLE {}.{} (\
                    id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,\
                    status VARCHAR(64) NOT NULL,\
                    in_match VARCHAR(8) NOT NULL,\
                    not_in_match VARCHAR(8) NOT NULL,\
                    empty_in_match VARCHAR(8) NOT NULL,\
                    empty_not_in_match VARCHAR(8) NOT NULL\
                )",
                qi(&cfg.project_schema),
                qi("inlist_rows")
            ))
            .await
            .expect("create MySQL inList proof table");

        let reg = std::collections::BTreeMap::from([("inlist_rows".to_string(), OWNER.to_string())]);
        let seed = r#"{"ir_version":1,"name":"seed_inlist_rows","ops":[
            {"op":"insert","table":"inlist_rows",
             "columns":["status","in_match","not_in_match","empty_in_match","empty_not_in_match"],
             "rows":[
                ["active","no","no","no","no"],
                ["trial","no","no","no","no"],
                ["deleted","no","no","no","no"],
                ["archived","no","no","no","no"]
             ]}
        ]}"#;
        lower_plan_and_apply_mysql(backend, cfg, seed, &reg).await;

        let updates = r#"{"ir_version":1,"name":"update_inlist_rows","ops":[
            {"op":"update","table":"inlist_rows",
             "set":{"in_match":{"node":"literal","value":"yes"}},
             "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":["active","trial"],"negated":false}},
            {"op":"update","table":"inlist_rows",
             "set":{"not_in_match":{"node":"literal","value":"yes"}},
             "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":["deleted","archived"],"negated":true}},
            {"op":"update","table":"inlist_rows",
             "set":{"empty_in_match":{"node":"literal","value":"yes"}},
             "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":[],"negated":false}},
            {"op":"update","table":"inlist_rows",
             "set":{"empty_not_in_match":{"node":"literal","value":"yes"}},
             "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":[],"negated":true}}
        ]}"#;
        lower_plan_and_apply_mysql(backend, cfg, updates, &reg).await;

        let rows = query(
            backend,
            &format!(
                "SELECT status, in_match, not_in_match, empty_in_match, empty_not_in_match \
                 FROM {}.{} ORDER BY status",
                qi(&cfg.project_schema),
                qi("inlist_rows")
            ),
            &[],
        )
        .await;
        let got = rows
            .rows
            .iter()
            .map(|row| {
                (
                    value_as_string(row.get("status")),
                    value_as_string(row.get("in_match")),
                    value_as_string(row.get("not_in_match")),
                    value_as_string(row.get("empty_in_match")),
                    value_as_string(row.get("empty_not_in_match")),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            got,
            vec![
                ("active".into(), "yes".into(), "yes".into(), "no".into(), "yes".into()),
                ("archived".into(), "no".into(), "no".into(), "no".into(), "yes".into()),
                ("deleted".into(), "no".into(), "no".into(), "no".into(), "yes".into()),
                ("trial".into(), "yes".into(), "yes".into(), "no".into(), "yes".into()),
            ],
            "MySQL inList/notIn/empty-list predicate matrix"
        );
    }));
}

#[test]
fn mysql_dml_step_rejects_destructive_out_of_scope() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "dml_scope", |backend, cfg, _account| Box::pin(async move {
        backend
            .exec(&format!(
                "CREATE TABLE {}.{} (id BIGINT NOT NULL PRIMARY KEY)",
                qi(&cfg.project_schema),
                qi("dml_scope")
            ))
            .await
            .expect("create DML scope table");
        backend
            .exec_with_binds(
                &format!(
                    "INSERT INTO {}.{} (id) VALUES (?)",
                    qi(&cfg.project_schema),
                    qi("dml_scope")
                ),
                &[BindValue::Int(1)],
            )
            .await
            .expect("seed DML scope row");

        let version = MigrationId::generate();
        let refused = backend
            .run_dml_step(
                cfg,
                &version,
                "wipe_dml_scope",
                &format!(
                    "DELETE FROM {}.{} WHERE id = ?",
                    qi(&cfg.project_schema),
                    qi("dml_scope")
                ),
                &[BindValue::Int(1)],
                true,
                OWNER,
                Approval::Approved,
                &ApprovalScope::Versions(std::collections::BTreeSet::new()),
                "mysql-jsdriver-e2e",
                LockMode::AlreadyHeld,
            )
            .await;
        assert!(
            matches!(
                refused,
                Err(ApplyError::ApprovalNotScoped { version: ref got })
                    if got == version.as_str()
            ),
            "destructive MySQL DML outside the approved version scope must be refused, got {refused:?}"
        );

        let rows = query(
            backend,
            &format!(
                "SELECT COUNT(*) AS n FROM {}.{}",
                qi(&cfg.project_schema),
                qi("dml_scope")
            ),
            &[],
        )
        .await;
        assert_eq!(
            value_as_i64(field(&rows.rows[0], "n")),
            1,
            "scope-refused destructive DML must not delete the row"
        );
        let journal = query(
            backend,
            &format!(
                "SELECT version FROM {}
                  WHERE event_kind = 'applied' AND phase = 'completed' AND version = ?",
                meta_table(cfg, "schema_migrations")
            ),
            &[BindValue::Text(version.as_str().to_string())],
        )
        .await;
        assert_eq!(rows_len(&journal), 0, "scope-refused DML must not journal");
    }));
}

#[test]
fn mysql_dml_step_is_idempotent_for_completed_version() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "dml_idem", |backend, cfg, _account| Box::pin(async move {
        backend
            .exec(&format!(
                "CREATE TABLE {}.{} (id BIGINT NOT NULL PRIMARY KEY, hits BIGINT NOT NULL)",
                qi(&cfg.project_schema),
                qi("dml_idem")
            ))
            .await
            .expect("create DML idempotency table");
        backend
            .exec_with_binds(
                &format!(
                    "INSERT INTO {}.{} (id, hits) VALUES (?, ?)",
                    qi(&cfg.project_schema),
                    qi("dml_idem")
                ),
                &[BindValue::Int(1), BindValue::Int(0)],
            )
            .await
            .expect("seed DML idempotency row");

        let version = MigrationId::generate();
        let template = format!(
            "UPDATE {}.{} SET hits = hits + ? WHERE id = ?",
            qi(&cfg.project_schema),
            qi("dml_idem")
        );
        let first = backend
            .run_dml_step(
                cfg,
                &version,
                "bump_dml_idem",
                &template,
                &[BindValue::Int(1), BindValue::Int(1)],
                false,
                OWNER,
                Approval::None,
                &ApprovalScope::All,
                "mysql-jsdriver-e2e",
                LockMode::AlreadyHeld,
            )
            .await
            .expect("first MySQL DML run should apply");
        assert!(first, "first DML run should report applied");

        let second = backend
            .run_dml_step(
                cfg,
                &version,
                "bump_dml_idem",
                &template,
                &[BindValue::Int(1), BindValue::Int(1)],
                false,
                OWNER,
                Approval::None,
                &ApprovalScope::All,
                "mysql-jsdriver-e2e",
                LockMode::AlreadyHeld,
            )
            .await
            .expect("second MySQL DML run should be a completed-version skip");
        assert!(!second, "completed DML version must return Ok(false) on retry");

        let rows = query(
            backend,
            &format!(
                "SELECT hits FROM {}.{} WHERE id = ?",
                qi(&cfg.project_schema),
                qi("dml_idem")
            ),
            &[BindValue::Int(1)],
        )
        .await;
        assert_eq!(
            value_as_i64(field(&rows.rows[0], "hits")),
            1,
            "completed-version retry must not execute the DML a second time"
        );
        let completed = backend.applied(cfg).await.expect("read MySQL applied journal");
        assert!(
            completed
                .iter()
                .any(|entry| entry.version == version.as_str() && entry.phase == Phase::Completed),
            "first DML run must journal the completed version"
        );
    }));
}

#[test]
fn mysql_dml_step_journals_execution_atomically() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };
    let live_for_closure = live.clone();

    run_isolated_mysql(&live, "dml_atomic", move |backend, cfg, _account| {
        let live = live_for_closure.clone();
        Box::pin(async move {
            backend
                .exec(&format!(
                    "CREATE TABLE {}.{} (id BIGINT NOT NULL PRIMARY KEY, label VARCHAR(64) NOT NULL)",
                    qi(&cfg.project_schema),
                    qi("dml_atomic")
                ))
                .await
                .expect("create DML atomic table");

            let admin = backend_for(&live);
            admin
                .exec(&format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} CHECK (name <> 'journal_failure')",
                    meta_table(cfg, "schema_migrations"),
                    qi("dml_atomic_block")
                ))
                .await
                .expect("install journal failure constraint for atomicity probe");

            let failed_version = MigrationId::generate();
            let failed_template = format!(
                "INSERT INTO {}.{} (id, label) VALUES (?, ?)",
                qi(&cfg.project_schema),
                qi("dml_atomic")
            );
            let failed = backend
                .run_dml_step(
                    cfg,
                    &failed_version,
                    "journal_failure",
                    &failed_template,
                    &[BindValue::Int(1), BindValue::Text("rolled_back".to_string())],
                    false,
                    OWNER,
                    Approval::None,
                    &ApprovalScope::All,
                    "mysql-jsdriver-e2e",
                    LockMode::AlreadyHeld,
                )
                .await;
            assert!(
                matches!(failed, Err(ApplyError::Journal(_))),
                "journal failure after DML must surface as a journal error, got {failed:?}"
            );
            let rolled_back_data = query(
                backend,
                &format!(
                    "SELECT label FROM {}.{} WHERE id = ?",
                    qi(&cfg.project_schema),
                    qi("dml_atomic")
                ),
                &[BindValue::Int(1)],
            )
            .await;
            assert_eq!(
                rows_len(&rolled_back_data),
                0,
                "journal failure must roll back the data DML in the same transaction"
            );
            let rolled_back_journal = query(
                backend,
                &format!(
                    "SELECT version FROM {}
                      WHERE event_kind = 'applied' AND version = ?",
                    meta_table(cfg, "schema_migrations")
                ),
                &[BindValue::Text(failed_version.as_str().to_string())],
            )
            .await;
            assert_eq!(
                rows_len(&rolled_back_journal),
                0,
                "failed journal insert must not leave a journal row"
            );

            let version = MigrationId::generate();
            let template = format!(
                "INSERT INTO {}.{} (id, label) VALUES (?, ?)",
                qi(&cfg.project_schema),
                qi("dml_atomic")
            );
            let ran = backend
                .run_dml_step(
                    cfg,
                    &version,
                    "insert_dml_atomic",
                    &template,
                    &[BindValue::Int(7), BindValue::Text("journaled".to_string())],
                    false,
                    OWNER,
                    Approval::None,
                    &ApprovalScope::All,
                    "mysql-jsdriver-e2e",
                    LockMode::AlreadyHeld,
                )
                .await
                .expect("MySQL DML should apply and journal in one transaction");
            assert!(ran, "fresh DML version should report applied");

            let data = query(
                backend,
                &format!(
                    "SELECT label FROM {}.{} WHERE id = ?",
                    qi(&cfg.project_schema),
                    qi("dml_atomic")
                ),
                &[BindValue::Int(7)],
            )
            .await;
            assert_eq!(rows_len(&data), 1, "DML data row must be committed");
            assert_eq!(field(&data.rows[0], "label"), &json!("journaled"));

            let expected_checksum = Checksum::of(&ChecksumInput {
                up: &template,
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: OWNER,
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            });
            let journal = query(
                backend,
                &format!(
                    "SELECT version, name, checksum, phase, kind
                       FROM {}
                      WHERE event_kind = 'applied' AND version = ?",
                    meta_table(cfg, "schema_migrations")
                ),
                &[BindValue::Text(version.as_str().to_string())],
            )
            .await;
            assert_eq!(rows_len(&journal), 1, "DML completed journal row must exist");
            let row = &journal.rows[0];
            assert_eq!(field(row, "version"), &json!(version.as_str()));
            assert_eq!(field(row, "name"), &json!("insert_dml_atomic"));
            assert_eq!(field(row, "checksum"), &json!(expected_checksum.as_str()));
            assert_eq!(field(row, "phase"), &json!("completed"));
            assert_eq!(field(row, "kind"), &json!("apply"));
        })
    });
}

#[test]
#[ignore = "helper launched by live_advisory_lock_serializes_concurrent_mysql_applies"]
fn mysql_advisory_lock_first_apply_helper() {
    if std::env::var("ZS_MYSQL_LOCK_HELPER").as_deref() != Ok("1") {
        return;
    }
    let _env = EnvGuard::set_dev();
    let host = std::env::var("ZS_MYSQL_LOCK_HELPER_HOST")
        .expect("helper host env");
    let port = std::env::var("ZS_MYSQL_LOCK_HELPER_PORT")
        .expect("helper port env")
        .parse::<u16>()
        .expect("helper port parses");
    let database = std::env::var("ZS_MYSQL_LOCK_HELPER_DATABASE")
        .expect("helper database env");
    let mut cfg = ExecutorConfig::new(
        std::env::var("ZS_MYSQL_LOCK_HELPER_PROJECT_ID")
            .expect("helper project id env"),
        std::env::var("ZS_MYSQL_LOCK_HELPER_PROJECT_SCHEMA")
            .expect("helper project schema env"),
    );
    cfg.pg.meta_schema = std::env::var("ZS_MYSQL_LOCK_HELPER_META_SCHEMA")
        .expect("helper meta schema env");
    let account = MysqlMigratorAccount {
        user: std::env::var("ZS_MYSQL_LOCK_HELPER_USER").expect("helper migrator user env"),
        host: "%".to_string(),
        password: std::env::var("ZS_MYSQL_LOCK_HELPER_PASSWORD")
            .expect("helper migrator password env"),
    };
    let live = LiveMysql {
        dsn: MysqlDsn {
            host,
            port,
            user: "root".to_string(),
            password: "zeroship".to_string(),
            database,
        },
        source: "lock helper env".to_string(),
    };

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let backend = migrator_backend_for(&live, &cfg, &account);
        backend
            .acquire_project_lock(&cfg)
            .await
            .expect("lock helper must acquire project GET_LOCK");
        backend
            .exec(&format!(
                "CREATE TABLE {}.{} (id INT NOT NULL)",
                qi(&cfg.project_schema),
                qi("lock_first")
            ))
            .await
            .expect("lock helper creates marker table while GET_LOCK is held");
        compio::time::sleep(Duration::from_secs(5)).await;
        backend
            .release_project_lock(&cfg)
            .await
            .expect("lock helper must release project GET_LOCK");
        println!("mysql_jsdriver_e2e lock helper held and released GET_LOCK");
    });
}

#[test]
fn live_advisory_lock_serializes_concurrent_mysql_applies() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };
    let live_for_closure = live.clone();

    run_isolated_mysql(&live, "lock", move |backend, cfg, account| {
        let live = live_for_closure.clone();
        Box::pin(async move {
            let second = guarded_create_table_migration(
                "lock_second_waits",
                &cfg.project_schema,
                "lock_second",
            );

            let mut short_cfg = cfg.clone();
            short_cfg.pg.lock_timeout = Duration::from_secs(1);
            let mut first_apply = spawn_lock_first_apply_helper(&live, cfg, account);
            if let Err(err) = wait_for_table_or_child_exit(
                backend,
                &cfg.project_schema,
                "lock_first",
                Duration::from_secs(10),
                &mut first_apply,
            )
            .await
            {
                let _ = first_apply.kill();
                wait_for_helper_success(first_apply, "startup after wait failure");
                panic!("{err}");
            }

            let started = Instant::now();
            let err = apply_one_result(
                backend,
                &short_cfg,
                &second,
                "mysql-jsdriver-e2e-lock-second",
            )
            .await
            .expect_err("second apply must fail closed while first holds GET_LOCK");
            assert_mysql_lock_timeout(err, started.elapsed());

            assert!(
                !table_exists(backend, &cfg.project_schema, "lock_second").await,
                "second migration must not enter the plan while GET_LOCK is held"
            );

            wait_for_helper_success(first_apply, "first apply completion");

            let retry = apply_one_result(
                backend,
                cfg,
                &second,
                "mysql-jsdriver-e2e-lock-second-retry",
            )
            .await
            .expect("second apply should proceed after first releases GET_LOCK");
            assert_eq!(retry.applied, vec![second.version.as_str().to_string()]);
            assert!(
                table_exists(backend, &cfg.project_schema, "lock_second").await,
                "second migration table should exist after retry"
            );
            println!("mysql_jsdriver_e2e advisory lock serialization assertions ran on real :3307");
        })
    });
}

#[test]
fn live_migrator_account_confines_privileges_and_reset_role_is_noop() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };
    let default_database = live.dsn.database.clone();

    run_isolated_mysql(&live, "confine", |backend, cfg, account| Box::pin(async move {
        let before = query(backend, "SELECT CURRENT_USER() AS current_user_name", &[]).await;
        let before_user = field(&before.rows[0], "current_user_name")
            .as_str()
            .expect("CURRENT_USER should be a string")
            .to_string();
        assert!(
            before_user.starts_with(&format!("{}@", account.user)),
            "migration connection must authenticate as {}, got {before_user}",
            account.user
        );

        backend.reset_role_best_effort().await;
        let after = query(backend, "SELECT CURRENT_USER() AS current_user_name", &[]).await;
        let after_user = field(&after.rows[0], "current_user_name")
            .as_str()
            .expect("CURRENT_USER should be a string");
        assert_eq!(
            after_user, before_user,
            "MySQL reset_role_best_effort must not change account identity"
        );

        backend
            .exec(&format!(
                "CREATE TABLE {}.{} (id INT NOT NULL)",
                qi(&cfg.project_schema),
                qi("in_scope_allowed")
            ))
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "in-scope project DDL should be allowed for {}: {err}",
                    account.user
                )
            });
        assert!(
            table_exists(backend, &cfg.project_schema, "in_scope_allowed").await,
            "in-scope project DDL should materialize"
        );

        let create_user = match backend
            .exec("CREATE USER 'zs_forbidden_e2e'@'%' IDENTIFIED BY 'nope'")
            .await
        {
            Ok(()) => panic!("{} must not be able to create users", account.user),
            Err(err) => err,
        };
        assert_mysql_access_denied(create_user, "CREATE USER");

        let cross_schema = match backend
            .exec(&format!(
                "CREATE TABLE {}.{} (id INT NOT NULL)",
                qi(&default_database),
                qi("zs_forbidden_cross_schema")
            ))
            .await
        {
            Ok(()) => panic!(
                "{} must not be able to create outside project schema",
                account.user
            ),
            Err(err) => err,
        };
        assert_mysql_access_denied(cross_schema, "cross-schema CREATE TABLE");

        println!(
            "mysql_jsdriver_e2e confinement assertions ran: denied privileged/cross-schema, allowed in-scope, current_user={after_user}"
        );
    }));
}

#[test]
fn live_tls_cold_cache_caching_sha2_connect_uses_pinned_ca() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };
    let live_for_closure = live.clone();

    run_isolated_mysql(&live, "tls", move |_backend, cfg, _account| {
        let live = live_for_closure.clone();
        Box::pin(async move {
            let admin = backend_for(&live);
            let account = provision_mysql_migrator_account_with_password(
                &admin,
                cfg,
                format!("zs_tls_{}", token("pwd")),
            )
            .await
            .expect("re-provision least-priv migrator account with fresh password");
            admin
                .exec("FLUSH PRIVILEGES")
                .await
                .expect("flush MySQL auth cache before cold-cache TLS connect");

            let ca_pem = mysql_server_ca_pem(&live);
            let tls_backend = tls_migrator_backend_for(&live, cfg, &account, ca_pem.clone());
            let current = query(
                &tls_backend,
                "SELECT CURRENT_USER() AS current_user_name",
                &[],
            )
            .await;
            let current_user = field(&current.rows[0], "current_user_name")
                .as_str()
                .expect("CURRENT_USER should be a string");
            assert!(
                current_user.starts_with(&format!("{}@", account.user)),
                "TLS cold-cache connection must authenticate as {}, got {current_user}",
                account.user
            );
            let ssl = query(&tls_backend, "SHOW SESSION STATUS LIKE 'Ssl_cipher'", &[]).await;
            assert_eq!(rows_len(&ssl), 1, "Ssl_cipher status row should exist");
            let cipher = field_any(&ssl.rows[0], &["Value", "value"])
                .as_str()
                .expect("Ssl_cipher Value should be a string");
            assert!(
                !cipher.is_empty(),
                "TLS connection must have a non-empty Ssl_cipher"
            );

            let empty_ca_err = tls_migrator_backend_result_for(&live, cfg, &account, "")
                .expect_err("empty CA must be rejected before opening the mysql2 TLS driver");
            assert!(
                empty_ca_err.to_string().contains("non-empty pinned ssl.ca"),
                "empty CA should fail with pin-required policy, got {empty_ca_err}"
            );
            let invalid_ca_backend =
                tls_migrator_backend_for(&live, cfg, &account, invalid_ca_pem());
            let invalid_ca_err = invalid_ca_backend
                .query_json("SELECT 1 AS ok", &[])
                .await
                .expect_err("invalid CA must be rejected by the mysql2 node:tls path");
            let unpinned_err = {
                let _native_roots = NativeRootsGuard::set(&ca_pem, "mysql-unpinned");
                tls_migrator_backend_without_ca_result_for(&live, cfg, &account).expect_err(
                    "TLS without pinned CA must be rejected even when native roots would trust the server",
                )
            };
            assert!(
                unpinned_err.to_string().contains("non-empty pinned ssl.ca"),
                "unpinned native-root path should fail with pin-required policy, got {unpinned_err}"
            );

            println!(
                "mysql_jsdriver_e2e TLS cold-cache caching_sha2 connect succeeded on real :3307: current_user={current_user} Ssl_cipher={cipher}; empty CA rejected={empty_ca_err}; invalid CA rejected={invalid_ca_err}; unpinned native-root path rejected={unpinned_err}"
            );
        })
    });
}

#[test]
fn live_mysql_rejects_raw_multi_statement_non_txn_at_validate_time() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "raw", |backend, cfg, _account| Box::pin(async move {
        let up = format!(
            "CREATE TABLE {}.{} (id INT NOT NULL);\nCREATE INDEX {} ON {}.{} (id)",
            qi(&cfg.project_schema),
            qi("phase_probe"),
            qi("phase_probe_missing_idx"),
            qi(&cfg.project_schema),
            qi("missing_table"),
        );
        let migration = manual_migration("raw_multi_statement_rejected", up);

        let err = MigrationEngine::new()
            .apply_verified(
                std::slice::from_ref(&migration),
                &GuardConfig::confined_mysql(cfg.project_schema.clone()),
                None,
                Approval::None,
                backend,
                cfg,
                "mysql-jsdriver-e2e",
            )
            .await
            .expect_err("raw multi-statement MySQL up must fail validation before DDL");
        assert!(
            matches!(err, EngineError::Apply(ApplyError::NonIdempotentNonTxn { .. })),
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
        assert_eq!(rows_len(&inflight), 0, "validation failure must not write started marker");
        assert_eq!(rows_len(&completed), 0, "completed marker must not be written");
        assert_eq!(rows_len(&table), 0, "validation failure must not run the first fragment");
        println!(
            "mysql_jsdriver_e2e raw multi-statement MySQL up rejected at validate-time on real :3307"
        );
    }));
}

#[test]
fn live_fragment_crash_recovery_replays_decide_per_fragment() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };
    let live_for_closure = live.clone();

    run_isolated_mysql(&live, "crash", move |backend, cfg, account| {
        let live = live_for_closure.clone();
        Box::pin(async move {
            let (migration, fragments) = crash_recovery_migration(cfg);
            let version = migration.version.as_str().to_string();

            let crash_backend = migrator_backend_for(&live, cfg, account);
            crash_backend.register_guarded_fragments(&version, fragments.clone());
            let connection_id = mysql_connection_id(&crash_backend).await;
            let killer = spawn_sleep_fragment_killer(live.clone(), connection_id, "SLEEP");

            let err = apply_one_result(
                &crash_backend,
                cfg,
                &migration,
                "mysql-jsdriver-e2e-crash",
            )
            .await
            .expect_err("external killer must close the connection mid-plan");
            killer
                .join()
                .expect("sleep-fragment killer helper should finish cleanly");
            assert!(
                matches!(
                    err,
                    EngineError::Apply(ApplyError::MigrationFailed { .. })
                        | EngineError::Apply(ApplyError::Db(_))
                ),
                "expected killed-connection apply error, got {err:?}"
            );

            assert!(
                table_exists(backend, &cfg.project_schema, "crash_users").await,
                "fragment 1 CREATE TABLE must be durable after the injected drop"
            );
            assert!(
                !index_exists(
                    backend,
                    &cfg.project_schema,
                    "crash_users",
                    "crash_users_email_idx"
                )
                .await,
                "fragment 2 CREATE INDEX must not have run before the injected drop"
            );
            let inflight = query(
                backend,
                &format!(
                    "SELECT version FROM {} WHERE version = ?",
                    meta_table(cfg, "schema_migrations_inflight")
                ),
                &[BindValue::Text(version.clone())],
            )
            .await;
            let completed = query(
                backend,
                &format!(
                    "SELECT version FROM {}
                      WHERE event_kind = 'applied' AND phase = 'completed' AND version = ?",
                    meta_table(cfg, "schema_migrations")
                ),
                &[BindValue::Text(version.clone())],
            )
            .await;
            assert_eq!(rows_len(&inflight), 1, "inflight marker must remain after crash");
            assert_eq!(rows_len(&completed), 0, "completed marker must not exist after crash");
            drop(crash_backend);
            wait_for_project_lock_released(backend, cfg).await;

            let recovery_backend = migrator_backend_for(&live, cfg, account);
            recovery_backend.register_guarded_fragments(&version, fragments);
            let recovered = apply_one_result(
                &recovery_backend,
                cfg,
                &migration,
                "mysql-jsdriver-e2e-recover",
            )
            .await
            .expect("recovery run should finish the missing fragment");
            assert_eq!(recovered.recovered, vec![version.clone()]);
            assert_eq!(recovered.applied, vec![version.clone()]);

            let inflight_after = query(
                backend,
                &format!(
                    "SELECT version FROM {} WHERE version = ?",
                    meta_table(cfg, "schema_migrations_inflight")
                ),
                &[BindValue::Text(version.clone())],
            )
            .await;
            let completed_after = query(
                backend,
                &format!(
                    "SELECT version FROM {}
                      WHERE event_kind = 'applied' AND phase = 'completed' AND version = ?",
                    meta_table(cfg, "schema_migrations")
                ),
                &[BindValue::Text(version.clone())],
            )
            .await;
            assert_eq!(rows_len(&inflight_after), 0, "recovery must clear inflight");
            assert_eq!(rows_len(&completed_after), 1, "recovery must write completed");
            assert!(
                table_exists(backend, &cfg.project_schema, "crash_users").await,
                "table must remain present after recovery"
            );
            assert!(
                index_exists(
                    backend,
                    &cfg.project_schema,
                    "crash_users",
                    "crash_users_email_idx"
                )
                .await,
                "index must be present after recovery"
            );
            println!(
                "mysql_jsdriver_e2e crash recovery replayed guarded fragments on real :3307 after killing connection {connection_id}: version={version}"
            );
        })
    });
}

#[test]
fn live_fragment_crash_recovery_faildrift_refuses_divergent_partial_apply() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };
    let live_for_closure = live.clone();

    run_isolated_mysql(&live, "faildrift", move |backend, cfg, account| {
        let live = live_for_closure.clone();
        Box::pin(async move {
            let (migration, fragments) = crash_recovery_faildrift_migration(cfg);
            let version = migration.version.as_str().to_string();

            let crash_backend = migrator_backend_for(&live, cfg, account);
            crash_backend.register_guarded_fragments(&version, fragments.clone());
            let connection_id = mysql_connection_id(&crash_backend).await;
            let killer = spawn_sleep_fragment_killer(live.clone(), connection_id, "SLEEP");

            let err = apply_one_result(
                &crash_backend,
                cfg,
                &migration,
                "mysql-jsdriver-e2e-faildrift-crash",
            )
            .await
            .expect_err("external killer must close the connection mid-plan");
            killer
                .join()
                .expect("sleep-fragment killer helper should finish cleanly");
            assert!(
                matches!(
                    err,
                    EngineError::Apply(ApplyError::MigrationFailed { .. })
                        | EngineError::Apply(ApplyError::Db(_))
                ),
                "expected killed-connection apply error, got {err:?}"
            );
            drop(crash_backend);
            wait_for_project_lock_released(backend, cfg).await;

            assert!(
                table_exists(backend, &cfg.project_schema, "drift_users").await,
                "fragment 1 CREATE TABLE must be durable after the injected drop"
            );
            assert!(
                !index_exists(
                    backend,
                    &cfg.project_schema,
                    "drift_users",
                    "drift_users_email_idx"
                )
                .await,
                "fragment 2 probe target must not exist before the divergent mutation"
            );

            backend
                .exec(&format!(
                    "CREATE INDEX {} ON {}.{} (id)",
                    qi("drift_users_email_idx"),
                    qi(&cfg.project_schema),
                    qi("drift_users")
                ))
                .await
                .expect("create divergent same-name index");

            let live_snapshot = backend
                .snapshot_schema(cfg)
                .await
                .expect("snapshot mutated MySQL schema");
            match decide(&fragments[1].existence_guard, &live_snapshot, SqlDialect::Mysql) {
                GuardVerdict::FailDrift(d) => {
                    assert_eq!(d.object, "index drift_users_email_idx");
                    assert_eq!(d.field, "columns");
                    assert_eq!(d.expected, "email");
                    assert_eq!(d.actual, "id");
                }
                other => panic!("expected fragment-2 probe to FailDrift, got {other:?}"),
            }

            let recovery_backend = migrator_backend_for(&live, cfg, account);
            recovery_backend.register_guarded_fragments(&version, fragments);
            let err = apply_one_result(
                &recovery_backend,
                cfg,
                &migration,
                "mysql-jsdriver-e2e-faildrift-recover",
            )
            .await
            .expect_err("recovery over a divergent fragment must fail closed");
            match err {
                EngineError::Apply(ApplyError::ExistenceGuardDrift {
                    object,
                    field,
                    expected,
                    actual,
                    ..
                }) => {
                    assert_eq!(object, "index drift_users_email_idx");
                    assert_eq!(field, "columns");
                    assert_eq!(expected, "email");
                    assert_eq!(actual, "id");
                }
                other => panic!("expected ExistenceGuardDrift, got {other:?}"),
            }

            let inflight_after = query(
                backend,
                &format!(
                    "SELECT version FROM {} WHERE version = ?",
                    meta_table(cfg, "schema_migrations_inflight")
                ),
                &[BindValue::Text(version.clone())],
            )
            .await;
            let completed_after = query(
                backend,
                &format!(
                    "SELECT version FROM {}
                      WHERE event_kind = 'applied' AND phase = 'completed' AND version = ?",
                    meta_table(cfg, "schema_migrations")
                ),
                &[BindValue::Text(version.clone())],
            )
            .await;
            assert_eq!(
                rows_len(&inflight_after),
                1,
                "FailDrift recovery must leave the migration inflight"
            );
            assert_eq!(
                rows_len(&completed_after),
                0,
                "FailDrift recovery must not write the completed marker"
            );
            println!(
                "mysql_jsdriver_e2e crash recovery FailDrift refused divergent partial apply on real :3307 after killing connection {connection_id}: version={version}"
            );
        })
    });
}

#[test]
fn live_snapshot_roundtrips_and_redeploy_is_noop() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "snapshot", |backend, cfg, _account| Box::pin(async move {
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
        assert_eq!(
            types,
            vec![
                ("active", "boolean", false),
                ("email", "text", false),
                ("id", "bigint", false),
                ("profile", "jsonb", true),
            ]
        );

        let redeploy = apply_fixture(&backend, &cfg, &migrations).await;
        assert!(redeploy.is_noop(), "same migration set should redeploy as skip: {redeploy:?}");
        assert_eq!(sorted_versions(redeploy.skipped), migration_versions(&migrations));
        println!("mysql_jsdriver_e2e snapshot + redeploy assertions ran");
    }));
}

#[test]
fn mysql_fk_default_action_does_not_false_drift() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    for (label, on_delete, on_update) in [
        ("fkdefault", None, None),
        ("fkrestrict", Some("restrict"), Some("restrict")),
        ("fknoaction", Some("noAction"), Some("noAction")),
    ] {
        run_isolated_mysql(&live, label, |backend, cfg, _account| Box::pin(async move {
            let ir = fk_default_action_ir(on_delete, on_update);
            let migrations =
                lower_mysql_migrations(&cfg.project_schema, &ir, "fk_default_action");
            apply_fixture(backend, cfg, &migrations).await;

            let live_snapshot = backend
                .snapshot_schema(cfg)
                .await
                .expect("snapshot live MySQL FK default-action schema");
            let expected = fold_ops(&ir.ops, SqlDialect::Mysql, &cfg.project_schema)
                .expect("fold default-action FK IR to expected MySQL snapshot");
            let drift = diff_snapshots(&expected, &live_snapshot);
            assert!(
                drift.is_clean(),
                "{label}: MySQL FK default/restrict/noAction must not false-drift: {drift:?}"
            );

            let orders = live_snapshot.tables.get("orders").expect("orders table");
            let fk = orders
                .constraints
                .iter()
                .find(|c| c.name == "orders_account_fkey")
                .expect("orders_account_fkey");
            assert!(
                !fk.definition.contains("ON DELETE RESTRICT")
                    && !fk.definition.contains("ON UPDATE RESTRICT")
                    && !fk.definition.contains("ON DELETE NO ACTION")
                    && !fk.definition.contains("ON UPDATE NO ACTION"),
                "{label}: MySQL default FK action must be implicit in canonical definition: {}",
                fk.definition
            );

            let redeploy = apply_fixture(backend, cfg, &migrations).await;
            assert!(
                redeploy.is_noop(),
                "{label}: same default-action FK migration set should redeploy as skip: {redeploy:?}"
            );
            assert_eq!(sorted_versions(redeploy.skipped), migration_versions(&migrations));
        }));
    }

    println!(
        "mysql_jsdriver_e2e FK default-action canonicalization assertions ran on real :3307"
    );
}

#[test]
fn live_mysql_fk_snapshot_roundtrips_actions_and_detects_action_drift() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let Some(live) = live_mysql_or_skip() else { return };

    run_isolated_mysql(&live, "fkdrift", |backend, cfg, _account| Box::pin(async move {
        let ir = fk_roundtrip_ir();
        let migrations = lower_mysql_migrations(&cfg.project_schema, &ir, "fk_roundtrip");
        apply_fixture(backend, cfg, &migrations).await;

        let live_snapshot = backend
            .snapshot_schema(cfg)
            .await
            .expect("snapshot live MySQL FK schema");
        let expected = fold_ops(&ir.ops, SqlDialect::Mysql, &cfg.project_schema)
            .expect("fold FK IR to expected MySQL snapshot");
        let drift = diff_snapshots(&expected, &live_snapshot);
        assert!(
            drift.is_clean(),
            "clean MySQL FK should match folded IR without false drift: {drift:?}"
        );

        let orders = live_snapshot.tables.get("orders").expect("orders table");
        let fk = orders
            .constraints
            .iter()
            .find(|c| c.name == "orders_account_fkey")
            .expect("orders_account_fkey");
        assert!(
            fk.definition.contains("REFERENCES"),
            "FK definition must include target: {}",
            fk.definition
        );
        assert!(
            fk.definition.contains("ON UPDATE CASCADE")
                && fk.definition.contains("ON DELETE SET NULL"),
            "FK definition must include actions: {}",
            fk.definition
        );

        let redeploy = apply_fixture(backend, cfg, &migrations).await;
        assert!(redeploy.is_noop(), "same FK migration set should redeploy as skip: {redeploy:?}");

        backend
            .exec(&format!(
                "ALTER TABLE {}.{} DROP FOREIGN KEY {}",
                qi(&cfg.project_schema),
                qi("orders"),
                qi("orders_account_fkey")
            ))
            .await
            .expect("drop FK before divergent re-add");
        backend
            .exec(&format!(
                "ALTER TABLE {}.{} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{} ({}) \
                 ON UPDATE CASCADE ON DELETE RESTRICT",
                qi(&cfg.project_schema),
                qi("orders"),
                qi("orders_account_fkey"),
                qi("account_id"),
                qi(&cfg.project_schema),
                qi("accounts"),
                qi("id")
            ))
            .await
            .expect("re-add FK with divergent action");

        let mutated_snapshot = backend
            .snapshot_schema(cfg)
            .await
            .expect("snapshot mutated MySQL FK schema");
        let drift = diff_snapshots(&expected, &mutated_snapshot);
        assert!(
            !drift.is_clean(),
            "changed MySQL FK action must be detected as drift"
        );
        println!(
            "mysql_jsdriver_e2e FK snapshot roundtrip and action drift assertions ran on real :3307: {drift:?}"
        );
    }));
}
