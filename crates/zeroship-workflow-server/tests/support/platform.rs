#![allow(
    clippy::future_not_send,
    reason = "fixture clients and connections stay on their compio runtime"
)]

use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

pub struct Platform {
    _postgres: Container<GenericImage>,
    pub admin: compio_postgres::Client,
    pub runtime_url: String,
    pub work: tempfile::TempDir,
    /// An `active` `zeroship.worker_enrollers` row this fixture seeds once, so
    /// every test that enrols a worker instance directly (option 1A froze
    /// `worker_instances.enroller_id` NOT NULL with a restrict FK to this
    /// table) has a satisfying value to reference without minting its own
    /// enroller per call site. Tests exercising enroller-level behaviour
    /// itself (revocation, a second enroller) seed their own rows instead.
    pub default_enroller_id: String,
}
impl Platform {
    pub async fn new() -> Self {
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("workflow host tests require Testcontainers PostgreSQL");
        let address = format!(
            "{}:{}",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        let url = format!("postgres://postgres@{address}/postgres");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        let config = work.path().join("migrate.toml");
        write_private(&config, toml::to_string(&serde_json::json!({"env":{"platform":{
            "url":url,"dir":root.join("db/migrations-ts"),"schema":"zeroship","owner_app":"zeroship_platform",
            "registry":root.join("policies/platform-table-owners.json"),"policy":[root.join("policies/platform.policy.toml")],
        }}})).unwrap());
        let result = Command::new("node")
            .arg(root.join("packages/zero-migrate-cli/dist/cli-bin.js"))
            .args(["apply", "--config"])
            .arg(&config)
            .args(["--env", "platform", "--approve"])
            .current_dir(&root)
            .output()
            .expect("build the canonical migration CLI before testing");
        assert!(
            result.status.success(),
            "platform migration failed:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let admin = connect(&url).await;
        let default_enroller_id = "wen_testfixturedefault0000000".to_string();
        admin
            .execute(
                "INSERT INTO zeroship.worker_enrollers (id, public_key, execution_zone_id, status) \
                 VALUES ($1, $2, 'ezn_default000000000000000000', 'active')",
                &[&default_enroller_id, &vec![7_u8; 32]],
            )
            .await
            .unwrap();
        Self {
            _postgres: postgres,
            admin,
            runtime_url: format!("postgres://zeroship_workflow@{address}/postgres"),
            work,
            default_enroller_id,
        }
    }
}
impl Platform {
    /// A live Control app in the deployment's seeded zone, created the way
    /// Control creates one, so placement can read its zone and deletion.
    /// Seeding an app that exists changes nothing.
    #[allow(dead_code, reason = "not every host contract places apps")]
    pub async fn seed_app(&self, app: &zeroship_core::AppId) {
        let exists = self
            .admin
            .query("SELECT 1 FROM zeroship.apps WHERE id=$1", &[&app.as_str()])
            .await
            .unwrap();
        if !exists.is_empty() {
            return;
        }
        let organization = zeroship_core::OrganizationId::mint();
        let project = zeroship_core::ProjectId::mint();
        let name = app.as_str().replace('_', "-");
        self.admin
            .execute(
                "INSERT INTO zeroship.organizations(id,slug,name,billing_email) \
                 VALUES($1,$2,'Placement Test','placement@zeroship.test')",
                &[&organization.as_str(), &name],
            )
            .await
            .unwrap();
        self.admin
            .execute(
                "INSERT INTO zeroship.projects(id,organization_id,slug,name) \
                 VALUES($1,$2,'default','Placement Test')",
                &[&project.as_str(), &organization.as_str()],
            )
            .await
            .unwrap();
        self.admin
            .execute(
                "INSERT INTO zeroship.apps(id,name,project_id,organization_id) VALUES($1,$2,$3,$4)",
                &[&app.as_str(), &name, &project.as_str(), &organization.as_str()],
            )
            .await
            .unwrap();
    }
}

pub async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .unwrap();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}
pub fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
