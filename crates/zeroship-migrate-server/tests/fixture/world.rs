//! The control-plane rows a test declares into, written exactly as
//! `zeroship_control::databases` writes them.
//!
//! One spelling, shared by every target that needs a world: the reconciler
//! target and the apply target both declare an organization, a project in a
//! zone, apps, databases and bindings, and a second copy of those INSERTs would
//! let one target's fixture drift into a shape the control surface never
//! produces.

use compio_postgres::Client;
use uuid::Uuid;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};

pub struct World {
    pub zone: String,
    pub organization: String,
    pub project: String,
}

impl World {
    /// One zone, one organization, one project, seeded directly.
    ///
    /// The zone is private to the test so the datastore this test registers is
    /// invisible to placement in any other test's zone.
    pub async fn new(pg: &Client, label: &str) -> Self {
        let unique = Uuid::new_v4().simple().to_string();
        let zone = zeroship_core::typed_id::generate("ezn");
        pg.execute(
            "INSERT INTO zeroship.execution_zones (id, name, status) \
             VALUES ($1, $2, 'active')",
            &[&zone, &format!("{label}-{unique}")],
        )
        .await
        .expect("declare this test's execution zone");

        pg.execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json) \
             VALUES ('free', 'free', '{}'::json) ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .expect("the plan an app row references");

        let organization = zeroship_core::typed_id::generate("org");
        pg.execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2, $3, $4)",
            &[
                &organization,
                &format!("{label}-{unique}"),
                &label,
                &format!("{label}-{unique}@example.test"),
            ],
        )
        .await
        .expect("seed this test's organization");

        let project = zeroship_core::typed_id::generate("prj");
        pg.execute(
            "INSERT INTO zeroship.projects \
                 (id, organization_id, slug, name, execution_zone_id) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &project,
                &organization,
                &format!("{label}-{unique}"),
                &label,
                &zone,
            ],
        )
        .await
        .expect("seed this test's project in this test's zone");

        Self {
            zone,
            organization,
            project,
        }
    }

    pub async fn app(&self, pg: &Client, label: &str) -> String {
        let app = zeroship_core::typed_id::generate("app");
        pg.execute(
            "INSERT INTO zeroship.apps \
                 (id, name, plan_id, project_id, organization_id, execution_zone_id) \
             VALUES ($1, $2, 'free', $3, $4, $5)",
            &[
                &app,
                &format!("{label}-{}", Uuid::new_v4().simple()),
                &self.project,
                &self.organization,
                &self.zone,
            ],
        )
        .await
        .expect("seed an app in this test's project");
        app
    }

    /// Declare a database, exactly as `zeroship_control::databases::create`
    /// does: `provisioning`, on a datastore placement chose.
    pub async fn declare_database(&self, pg: &Client, datastore: &str, name: &str) -> DatabaseId {
        let database = DatabaseId::mint();
        pg.execute(
            "INSERT INTO zeroship.databases \
                 (id, project_id, execution_zone_id, datastore_id, name, status) \
             VALUES ($1, $2, $3, $4, $5, 'provisioning')",
            &[
                &database.as_str(),
                &self.project,
                &self.zone,
                &datastore,
                &name,
            ],
        )
        .await
        .expect("declare a database on this test's datastore");
        database
    }

    /// Declare a binding, exactly as `zeroship_control::databases::bind` does.
    pub async fn declare_binding(
        &self,
        pg: &Client,
        app: &str,
        database: &DatabaseId,
        capability: DatabaseCapability,
    ) -> BindingId {
        let binding = BindingId::mint();
        pg.execute(
            "INSERT INTO zeroship.database_bindings \
                 (id, app_id, database_id, project_id, capability, status) \
             VALUES ($1, $2, $3, $4, $5, 'pending')",
            &[
                &binding.as_str(),
                &app,
                &database.as_str(),
                &self.project,
                &capability.as_wire(),
            ],
        )
        .await
        .expect("declare a binding in this test's project");
        binding
    }
}
