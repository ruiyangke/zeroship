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

/// The single execution zone these migrations seed
/// (`db/migrations-ts/20260914000450_execution_zones_default_zone.ts`).
pub const DEFAULT_ZONE_ID: &str = "ezn_default000000000000000000";

pub struct Platform {
    _postgres: Container<GenericImage>,
    pub admin: compio_postgres::Client,
    pub runtime_url: String,
    pub work: tempfile::TempDir,
    /// An `active` `zeroship.worker_join_signers` row this fixture seeds once,
    /// so every test that joins a worker instance directly (`worker_instances.
    /// join_signer_id` is NOT NULL with a restrict FK to this table) has a
    /// satisfying value to reference without minting its own signer per call
    /// site. Tests exercising signer-level behaviour itself (revocation, a
    /// second signer) seed their own rows instead.
    #[allow(
        dead_code,
        reason = "Control's deployment-hold suite includes this file and joins through a \
                  signer whose private key it holds, so it never reads this row"
    )]
    pub default_join_signer_id: String,
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
        let default_join_signer_id = "wjs_testfixturedefault0000000".to_string();
        admin
            .execute(
                "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
                 VALUES ($1, $2, 'active')",
                &[&default_join_signer_id, &vec![7_u8; 32]],
            )
            .await
            .unwrap();
        admin
            .execute(
                "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
                 VALUES ($1, 'ezn_default000000000000000000')",
                &[&default_join_signer_id],
            )
            .await
            .unwrap();
        Self {
            _postgres: postgres,
            admin,
            runtime_url: format!("postgres://zeroship_workflow@{address}/postgres"),
            work,
            default_join_signer_id,
        }
    }
}
impl Platform {
    /// A live Control app in the deployment's seeded zone, created the way
    /// Control creates one, so placement can read its zone and deletion.
    /// Seeding an app that exists changes nothing.
    #[allow(dead_code, reason = "not every host contract places apps")]
    pub async fn seed_app(&self, app: &zeroship_core::AppId) -> String {
        self.seed_app_in(app, None).await
    }

    /// As [`Self::seed_app`], in `zone` when given. An app's zone is fixed
    /// when the app is created. Returns the app's plan, which allows
    /// workflows.
    #[allow(dead_code, reason = "not every host contract places apps")]
    pub async fn seed_app_in(&self, app: &zeroship_core::AppId, zone: Option<&str>) -> String {
        let existing = self
            .admin
            .query("SELECT plan_id FROM zeroship.apps WHERE id=$1", &[&app.as_str()])
            .await
            .unwrap();
        if let Some(row) = existing.first() {
            return row.get(0);
        }
        let organization = zeroship_core::OrganizationId::mint();
        let project = zeroship_core::ProjectId::mint();
        let name = app.as_str().replace('_', "-");
        let plan = zeroship_core::typed_id::new_plan_id();
        self.admin
            .execute(
                "INSERT INTO zeroship.plans(id,name,runtime_limits_json,workflows_allowed) \
                 VALUES($1,$2,'{}',true)",
                &[&plan, &format!("plan-{name}")],
            )
            .await
            .unwrap();
        self.admin
            .execute(
                "INSERT INTO zeroship.organizations(id,slug,name,billing_email) \
                 VALUES($1,$2,'Placement Test','placement@zeroship.test')",
                &[&organization.as_str(), &name],
            )
            .await
            .unwrap();
        // The zone is always named: the column carries no default, so Control
        // resolves the deployment's one zone when a caller names none.
        let zone = zone.unwrap_or(DEFAULT_ZONE_ID);
        // The project is seeded in the SAME zone as the app below.
        // `apps_project_zone_fkey` requires an app's zone copy to equal its
        // project's, and both rows freeze their zone by trigger once written,
        // so the pair has to agree at INSERT and cannot be reconciled after.
        self.admin
            .execute(
                "INSERT INTO zeroship.projects(id,organization_id,slug,name,execution_zone_id) \
                 VALUES($1,$2,'default','Placement Test',$3)",
                &[&project.as_str(), &organization.as_str(), &zone],
            )
            .await
            .unwrap();
        let inserted = self
            .admin
            .execute(
                "INSERT INTO zeroship.apps(id,name,plan_id,project_id,organization_id,execution_zone_id) \
                 VALUES($1,$2,$3,$4,$5,$6)",
                &[&app.as_str(), &name, &plan, &project.as_str(), &organization.as_str(), &zone],
            )
            .await;
        assert_eq!(inserted.unwrap(), 1);
        plan
    }

    /// The placement row the manager's selection lane would commit, seeded
    /// directly. Contracts that place apps through selection use the manager;
    /// the job, policy and registry endpoints need only a live placement to
    /// authenticate against, and selection needs claimable work this fixture
    /// has no reason to publish.
    #[allow(dead_code, reason = "not every host contract holds a placement")]
    pub async fn seed_placement(
        &self,
        app: &zeroship_core::AppId,
        worker: &zeroship_core::workflow_coordination::WorkerId,
        ttl: std::time::Duration,
    ) -> zeroship_core::workflow_coordination::Assignment {
        let expires = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                + ttl.as_millis(),
        )
        .unwrap();
        self.admin
            .execute(
                "INSERT INTO workflow_manager.queue_scopes(id) VALUES($1) ON CONFLICT DO NOTHING",
                &[&app.as_str()],
            )
            .await
            .unwrap();
        let inserted = self
            .admin
            .execute(
                "INSERT INTO workflow_manager.assignments\
                 (id,app_id,worker_id,revision,expires_at,released,refused) \
                 VALUES($1,$2,$3,1,$4,false,false)",
                &[
                    &zeroship_core::typed_id::generate("wca"),
                    &app.as_str(),
                    &worker.as_str(),
                    &expires,
                ],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        zeroship_core::workflow_coordination::Assignment {
            app_id: app.clone(),
            worker_id: worker.clone(),
            revision: 1.try_into().unwrap(),
            expires_at: expires.try_into().unwrap(),
        }
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
