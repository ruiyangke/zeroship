#![allow(
    clippy::future_not_send,
    reason = "fixture clients and connections stay on their compio runtime"
)]

use std::{os::unix::fs::PermissionsExt, path::Path};

use zeroship_testkit::postgres::FreshDatabase;

/// The single execution zone these migrations seed
/// (`db/migrations-ts/20260914000450_execution_zones_default_zone.ts`).
pub const DEFAULT_ZONE_ID: &str = "ezn_default000000000000000000";
/// The `active` `zeroship.worker_join_signers` row [`Platform::new`] seeds, so
/// a contract that joins a worker instance directly has a satisfying value to
/// reference without minting a signer per call site. Contracts that exercise
/// signer behaviour itself (revocation, a second signer) seed their own rows.
pub const DEFAULT_JOIN_SIGNER_ID: &str = "wjs_testfixturedefault0000000";

/// A case's view of the platform database.
///
/// Every case of a run shares one migrated server through
/// [`zeroship_testkit::postgres::platform`]: [`Platform::new`] joins the
/// process-shared migrated database, and [`Platform::fresh_database`] takes a
/// clone of the migrated template for a subject that is platform-global. No
/// case boots a container or runs the platform migration.
pub struct Platform {
    /// The clone a fresh case owns, removed when this handle drops; `None` for
    /// a case on the process-shared database.
    _fresh: Option<FreshDatabase>,
    /// The superuser connection this case seeds and reads its own rows with.
    pub admin: compio_postgres::Client,
    /// The case database's URL as its superuser, `postgres`.
    pub admin_url: url::Url,
    /// The case database's URL as the workflow service login.
    pub runtime_url: String,
    /// The case's own scratch directory, for peer files, logs and payloads.
    pub work: tempfile::TempDir,
}

impl Platform {
    /// Join the process-shared migrated database.
    ///
    /// For a case whose subject is scoped to the rows it mints: it mints its
    /// own app, organization, project, plan, worker and zone ids, and reads
    /// back by those.
    pub async fn new() -> Self {
        let admin_url = zeroship_testkit::postgres::platform().admin_url();
        Box::pin(Self::open(admin_url, None)).await
    }

    /// Take a clone of the migrated template from the process-shared server.
    ///
    /// For a case whose subject is platform-global rather than scoped to the
    /// rows it mints, so isolating it in a database keeps it from another
    /// case's rows.
    pub async fn fresh_database() -> Self {
        let fresh = zeroship_testkit::postgres::platform().fresh_database();
        let admin_url = fresh.admin_url();
        Box::pin(Self::open(admin_url, Some(fresh))).await
    }

    async fn open(admin_url: url::Url, fresh: Option<FreshDatabase>) -> Self {
        let admin = connect(admin_url.as_str()).await;
        seed_default_join_signer(&admin).await;
        let runtime_url = runtime_url(&admin_url);
        Self {
            _fresh: fresh,
            admin,
            admin_url,
            runtime_url,
            work: tempfile::tempdir().unwrap(),
        }
    }

    /// The server's URL as `role`, whose own name is its password.
    pub fn role_url(&self, role: &str) -> url::Url {
        let mut url = self.admin_url.clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        url
    }

    /// Whether this case owns a clone of the migrated template rather than the
    /// process-shared working database.
    ///
    /// A subject that is platform-global and not scoped to the rows it mints -
    /// the deployment's execution zones, an installation-wide sweep - requires
    /// a clone. A fixture that reaches the process-shared database from such a
    /// subject leaks a row every sibling case then observes.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        self._fresh.is_some()
    }
}
impl Platform {
    /// A live Control app in the deployment's seeded zone, created the way
    /// Control creates one, so placement can read its zone and deletion.
    /// Seeding an app that exists changes nothing.
    pub async fn seed_app(&self, app: &zeroship_core::AppId) -> String {
        self.seed_app_in(app, None).await
    }

    /// As [`Self::seed_app`], in `zone` when given. An app's zone is fixed
    /// when the app is created. Returns the app's plan, which allows
    /// workflows.
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
}

/// Seed the join signer the fixture's default zone trusts, once per database.
///
/// A shared database receives this insert from every case that joins it, so it
/// is idempotent and a concurrent writer that loses the race is absorbed.
async fn seed_default_join_signer(admin: &compio_postgres::Client) {
    admin
        .execute(
            "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
             VALUES ($1, $2, 'active') ON CONFLICT DO NOTHING",
            &[&DEFAULT_JOIN_SIGNER_ID, &vec![7_u8; 32]],
        )
        .await
        .unwrap();
    admin
        .execute(
            "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
             VALUES ($1, 'ezn_default000000000000000000') ON CONFLICT DO NOTHING",
            &[&DEFAULT_JOIN_SIGNER_ID],
        )
        .await
        .unwrap();
}

/// The case database's URL as the workflow service login, whose password is
/// its own name.
fn runtime_url(admin_url: &url::Url) -> String {
    let mut url = admin_url.clone();
    url.set_username("zeroship_workflow").unwrap();
    url.set_password(Some("zeroship_workflow")).unwrap();
    url.to_string()
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

impl std::fmt::Debug for Platform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Platform").finish_non_exhaustive()
    }
}
