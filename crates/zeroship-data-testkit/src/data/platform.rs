//! The Control schema the CDC relay reads, built by the real platform corpus,
//! and the logins a deployment gives the relay and the worker.
//!
//! The relay reads Control's rows as `zeroship_cdc`, and what that login may
//! read is exactly what `db/migrations-ts` grants it. A fixture that declared
//! stand-in tables, or a relay role of its own with grants of its own, would
//! measure the grants the fixture wrote rather than the ones a deployment has,
//! and a relay whose reads outgrew the platform's grants would stay green.
//!
//! The caller owns the server; this module applies the corpus to it. The rows
//! it declares are written as the superuser: [`declare_edge`] inserts the
//! Control rows an edge consists of directly, and [`enroll_worker`] calls
//! `zeroship.join_worker_instance`, the function Control's join handler admits
//! with, from the superuser rather than from Control's login. What these rows
//! are is the platform's; who writes them is not what the relay's tests are
//! about, and only the relay's own reads run under a production login.

use compio_postgres::Pool;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use zeroship_core::{BindingId, DatabaseId, OrganizationId, ProjectId};

/// The relay's login, created with this development password by
/// `db/migrations-ts/20260702000100_schema_roles_extensions.ts`.
pub const RELAY_LOGIN: &str = "zeroship_cdc";

/// The worker's login, from the same migration. It holds no replication
/// attribute and reaches a tenant schema only by assuming a binding role.
pub const WORKER_LOGIN: &str = "zeroship_worker";

/// The execution zone the corpus seeds
/// (`db/migrations-ts/20260914000450_execution_zones_default_zone.ts`).
pub const DEFAULT_ZONE: &str = "ezn_default000000000000000000";

/// A server the caller owns, holding the platform schema the corpus builds.
#[derive(Debug)]
pub struct Platform {
    admin_url: String,
}

impl Platform {
    /// Apply `db/migrations-ts` to the server `admin_url` names, through the
    /// canonical migration CLI, as the operator's platform environment does.
    pub fn apply(admin_url: String) -> Self {
        apply_corpus(&admin_url);
        Self { admin_url }
    }

    /// Name a server whose platform corpus is already applied.
    ///
    /// [`apply`](Self::apply) runs the migration; this assumes a migrated
    /// server - a case database cloned from the platform template - and only
    /// names it, so the fixture's schema and logins are used as they stand.
    pub fn at(admin_url: String) -> Self {
        Self { admin_url }
    }

    /// The superuser the fixture declares rows and tenant objects with. Relay
    /// code is never handed this login.
    pub fn admin_url(&self) -> String {
        self.admin_url.clone()
    }

    /// The relay's production login.
    pub fn relay_url(&self) -> String {
        self.login_url(RELAY_LOGIN)
    }

    /// The worker's production login.
    pub fn worker_url(&self) -> String {
        self.login_url(WORKER_LOGIN)
    }

    fn login_url(&self, login: &str) -> String {
        let mut url = url::Url::parse(&self.admin_url).expect("fixture URL");
        url.set_username(login).expect("login name");
        url.set_password(Some(login)).expect("login password");
        url.into()
    }
}

/// [`declare_edge`] under a freshly minted binding id, which it returns.
pub async fn declare_binding(
    admin: &Pool,
    app: &str,
    database: &DatabaseId,
    status: &str,
) -> BindingId {
    let binding = BindingId::mint();
    declare_edge(admin, app, database, &binding, status).await;
    binding
}

/// Declare Control's rows for one `(app, database)` edge in the shape Control
/// holds them - the app in a project, the database in that project on this
/// cluster's datastore, and the binding between the two under `binding` -
/// inserted directly by the superuser.
///
/// Only the binding's `status` is chosen. The database is `active` and the
/// binding's `observed_generation` has caught up to its `generation`, so a
/// status other than `active` varies exactly one conjunct of the liveness
/// predicate. An app or database already declared is reused, so one app can
/// be given several databases.
pub async fn declare_edge(
    admin: &Pool,
    app: &str,
    database: &DatabaseId,
    binding: &BindingId,
    status: &str,
) {
    // One datastore per cluster, keyed on the cluster's own identity the way
    // the reconciler registers it.
    admin
        .execute(
            "INSERT INTO zeroship.datastores (id, system_identifier, execution_zone_id, status) \
             SELECT $1, system_identifier, $2, 'active' FROM pg_control_system() \
             ON CONFLICT (system_identifier) DO NOTHING",
            &[&zeroship_core::typed_id::generate("dst"), &DEFAULT_ZONE],
        )
        .await
        .expect("register this cluster's datastore");
    let project = app_project(admin, app).await;
    admin
        .execute(
            "INSERT INTO zeroship.databases \
               (id, project_id, execution_zone_id, datastore_id, name, status) \
             SELECT $1, $2, $3, d.id, $1, 'active' FROM zeroship.datastores d \
              WHERE d.system_identifier = (SELECT system_identifier FROM pg_control_system()) \
             ON CONFLICT (id) DO NOTHING",
            &[&database.as_str(), &project, &DEFAULT_ZONE],
        )
        .await
        .expect("declare the database");
    let declared = admin
        .execute(
            "INSERT INTO zeroship.database_bindings \
               (id, app_id, database_id, project_id, capability, status, generation, observed_generation) \
             VALUES ($1, $2, $3, $4, 'readwrite', $5, 1, 1)",
            &[&binding.as_str(), &app, &database.as_str(), &project, &status],
        )
        .await
        .expect("declare the binding");
    assert_eq!(declared, 1, "the binding row must be written");
}

/// The columns of `zeroship.<table>` the relay login may read, from the
/// catalog, so a test that withdraws the grant restores exactly what the
/// platform migrations granted.
pub async fn relay_columns(admin: &Pool, table: &str) -> Vec<String> {
    admin
        .query(
            "SELECT attname::text FROM pg_attribute \
             WHERE attrelid = $1::text::regclass AND attnum > 0 AND NOT attisdropped \
               AND has_column_privilege($2::text, attrelid, attnum, 'SELECT') \
             ORDER BY attnum",
            &[&format!("zeroship.{table}"), &RELAY_LOGIN],
        )
        .await
        .expect("read the relay's columns")
        .iter()
        .map(|row| row.try_get(0).expect("the column name decodes"))
        .collect()
}

/// Withdraw every privilege the relay login holds on `zeroship.<table>`,
/// table-wide and on each column.
pub async fn revoke_relay(admin: &Pool, table: &str) {
    admin
        .batch_execute(&format!(
            "REVOKE ALL ON zeroship.\"{table}\" FROM \"{RELAY_LOGIN}\""
        ))
        .await
        .expect("withdraw the relay's grant");
}

/// Grant the relay login `SELECT` on exactly `columns` of `zeroship.<table>`.
pub async fn grant_relay(admin: &Pool, table: &str, columns: &[String]) {
    assert!(!columns.is_empty(), "a grant names at least one column");
    let columns = columns
        .iter()
        .map(|column| format!("\"{column}\""))
        .collect::<Vec<_>>()
        .join(", ");
    admin
        .batch_execute(&format!(
            "GRANT SELECT ({columns}) ON zeroship.\"{table}\" TO \"{RELAY_LOGIN}\""
        ))
        .await
        .expect("restore the relay's grant");
}

/// Enrol one worker instance under a join signer of its own, returning the
/// signer so a caller can purge it.
///
/// The signer is trusted for the corpus's zone, and the instance is admitted
/// through `zeroship.join_worker_instance`, the function Control's join
/// handler admits every instance with, called here by the superuser.
pub async fn enroll_worker(admin: &Pool, instance: &str, public_key: &[u8; 32]) -> String {
    let signer = zeroship_core::typed_id::new_join_signer_id();
    let signer_key = zeroship_core::service_assertion::ServiceSigningKey::generate()
        .verifying_key_bytes()
        .to_vec();
    admin
        .execute(
            "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
             VALUES ($1, $2, 'active')",
            &[&signer, &signer_key],
        )
        .await
        .expect("declare the join signer");
    admin
        .execute(
            "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
             VALUES ($1, $2)",
            &[&signer, &DEFAULT_ZONE],
        )
        .await
        .expect("trust the signer for the zone");
    let joined: String = admin
        .query(
            "SELECT zeroship.join_worker_instance($1, $2, $3, $4, $5, $5, '127.0.0.1'::inet, 8080, 3600)",
            &[
                &signer,
                &zeroship_core::typed_id::generate("tst"),
                &DEFAULT_ZONE,
                &instance,
                &public_key.to_vec(),
            ],
        )
        .await
        .expect("admit the worker instance")
        .first()
        .expect("the join answers the admitted id")
        .try_get(0)
        .expect("the admitted id decodes");
    assert_eq!(
        joined, instance,
        "the instance is admitted under its own id"
    );
    signer
}

/// The project `app` lives in, declaring the organization, project and app on
/// first sight.
async fn app_project(admin: &Pool, app: &str) -> String {
    if let Some(row) = admin
        .query(
            "SELECT project_id FROM zeroship.apps WHERE id = $1",
            &[&app],
        )
        .await
        .expect("look the app up")
        .first()
    {
        return row.try_get(0).expect("the project id decodes");
    }
    let organization = OrganizationId::mint();
    let project = ProjectId::mint();
    let slug = app.replace('_', "-");
    admin
        .execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json) \
             VALUES ('free', 'Free', '{}') ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .expect("declare the default plan");
    admin
        .execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2, 'CDC relay', 'billing@cdc-relay.test')",
            &[&organization.as_str(), &slug],
        )
        .await
        .expect("declare the organization");
    admin
        .execute(
            "INSERT INTO zeroship.projects (id, organization_id, slug, name, execution_zone_id) \
             VALUES ($1, $2, 'relay', 'Relay', $3)",
            &[&project.as_str(), &organization.as_str(), &DEFAULT_ZONE],
        )
        .await
        .expect("declare the project");
    admin
        .execute(
            "INSERT INTO zeroship.apps (id, name, project_id, organization_id, execution_zone_id) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &app,
                &slug,
                &project.as_str(),
                &organization.as_str(),
                &DEFAULT_ZONE,
            ],
        )
        .await
        .expect("declare the app");
    project.as_str().to_owned()
}

fn apply_corpus(url: &str) {
    let root = root();
    let cli = root.join("packages/zero-migrate-cli/dist/cli-bin.js");
    assert!(
        cli.is_file(),
        "the CDC relay's tests read the platform schema; `cargo xtask test data` builds \
         the migration host they apply it with (as does `pnpm build`)"
    );
    let corpus = root.join("db/migrations-ts");
    let registry = root.join("policies/platform-table-owners.json");
    let policy = root.join("policies/platform.policy.toml");
    let environment = format!(
        "[env.platform]\nurl = {}\ndir = {}\nschema = \"zeroship\"\nowner_app = \"zeroship_platform\"\n\
         registry = {}\npolicy = [{}]\n",
        toml_string(url),
        toml_string(corpus.to_str().expect("a UTF-8 corpus path")),
        toml_string(registry.to_str().expect("a UTF-8 registry path")),
        toml_string(policy.to_str().expect("a UTF-8 policy path")),
    );
    // An owner-only file, so the credential never reaches the child's arguments.
    let config = tempfile::NamedTempFile::new().expect("private migration config");
    std::fs::write(config.path(), environment).expect("write migration config");
    let mut stdout = tempfile::tempfile().expect("migration stdout");
    let mut stderr = tempfile::tempfile().expect("migration stderr");
    let mut command = Command::new("node");
    remove_deployment_overrides(&mut command);
    let mut child = OwnedChild(
        command
            .current_dir(&root)
            .arg(&cli)
            .args(["apply", "--config"])
            .arg(config.path())
            .args(["--env", "platform", "--approve"])
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .expect("the CDC relay's tests apply the platform corpus through Node"),
    );
    let deadline = Instant::now() + Duration::from_mins(5);
    let outcome = loop {
        if let Some(outcome) = child.0.try_wait().expect("wait for the migration CLI") {
            break outcome;
        }
        assert!(
            Instant::now() < deadline,
            "platform migrations timed out: {}",
            read(&mut stderr)
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        outcome.success(),
        "platform migrations failed ({outcome}):\n{}\n{}",
        read(&mut stderr),
        read(&mut stdout)
    );
}

/// A TOML basic string. The values are a DSN and repository paths, so the two
/// characters a basic string must escape are the only ones handled.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("a test owner lives under crates/")
        .to_owned()
}

/// The CLI's environment outranks its config, so a developer's deployment
/// settings would otherwise redirect this apply to another database.
#[allow(
    clippy::disallowed_methods,
    reason = "a private fixture removes inherited deployment settings from a child; no application config is read"
)]
fn remove_deployment_overrides(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("ZERO_MIGRATE_")
            || name.starts_with("PG")
            || matches!(
                name.as_ref(),
                "DATABASE_URL"
                    | "NODE_OPTIONS"
                    | "NAPI_RS_NATIVE_LIBRARY_PATH"
                    | "NAPI_RS_FORCE_WASI"
            )
        {
            command.env_remove(key);
        }
    }
}

fn read(file: &mut std::fs::File) -> String {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut output = String::new();
    file.read_to_string(&mut output).unwrap();
    output
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
