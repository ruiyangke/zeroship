use compio_postgres::Client;
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_migrate_server::provisioning::provision_workflow_journal_schema;
use zeroship_workflow::store::pg::{PgStore, WorkflowTables};

use crate::test_database::Database;

pub(super) struct Journal<'db> {
    admin: &'db Client,
    tables: WorkflowTables,
}

impl<'db> Journal<'db> {
    pub async fn new(database: &'db Database, app_id: &AppId) -> Self {
        let conn = &database.admin;
        provision_workflow_journal_schema(conn, app_id)
            .await
            .expect("migration service provisions journal schema");
        let worker = database.connect_as("zeroship_worker").await;
        PgStore::provision(&worker, app_id)
            .await
            .expect("worker provisions its journal inside the owned schema");
        conn.execute("INSERT INTO zeroship.plans (id, name, workflows_allowed, runtime_limits_json) VALUES ('workflow', 'Workflow Fixture', true, '{}')", &[]).await.unwrap();
        let organization_id = typed_id::generate("org");
        let project_id = typed_id::generate("prj");
        conn.execute("INSERT INTO zeroship.organizations (id, slug, name, billing_email) VALUES ($1, 'workflow', 'Workflow Fixture', 'fixture@zeroship.test')", &[&organization_id]).await.unwrap();
        conn.execute("INSERT INTO zeroship.projects (id, organization_id, slug, name) VALUES ($1, $2, 'default', 'Default')", &[&project_id, &organization_id]).await.unwrap();
        conn.execute("INSERT INTO zeroship.apps (id, name, plan_id, workflows_enabled, project_id, organization_id) VALUES ($1, 'workflow', 'workflow', true, $2, $3)", &[&app_id.as_str(), &project_id, &organization_id]).await.unwrap();
        Self {
            admin: conn,
            tables: WorkflowTables::for_app_id(app_id),
        }
    }

    pub async fn record_deploy(&self, hash: &str) {
        let id = typed_id::generate("dep");
        self.admin.execute("INSERT INTO zeroship.app_deploys (id, app_id, deploy_hash, manifest_json, activated_at) VALUES ($1, $2, $3, '{}', now())", &[&id, &self.tables.app_id.as_str(), &hash]).await.unwrap();
    }

    pub async fn seed_run(&self, run_id: &str, workflow: &str, hash: &str) {
        let deploy: String = self
            .admin
            .query_one(
                "SELECT id FROM zeroship.app_deploys WHERE app_id = $1 AND deploy_hash = $2",
                &[&self.tables.app_id.as_str(), &hash],
            )
            .await
            .unwrap()
            .get(0);
        self.admin.execute(&format!("INSERT INTO {} (id, workflow_name, app_id, deploy_id, state, input, started_at, wake_at) VALUES ($1, $2, $3, $4, 'queued', $5, now(), now())", self.tables.runs), &[&run_id, &workflow, &self.tables.app_id.as_str(), &deploy, &serde_json::json!({"orderId": "ord_1"})]).await.expect("seed unclaimed workflow run");
    }
    pub async fn reclaim(&self, run_id: &str) {
        let conn = self.admin;
        let tables = &self.tables;
        let affected = conn.execute(
            &format!(
                "UPDATE {} \
                    SET state = 'running', claimed_by = NULL, dispatch_nonce = NULL, lease_expires = NULL, wake_at = now() \
                  WHERE id = $1",
                tables.runs
            ),
            &[&run_id],
        )
        .await
        .expect("reclaim workflow run for replay");
        assert_eq!(affected, 1, "reclaim the seeded run");
    }

    pub async fn steal_claim(&self, run_id: &str) {
        let conn = self.admin;
        let tables = &self.tables;
        let affected = conn.execute(
            &format!(
                "UPDATE {} \
                    SET state = 'running', claimed_by = 'other-worker', dispatch_nonce = 'wfd_other', lease_expires = now() + interval '1 minute' \
                  WHERE id = $1",
                tables.runs
            ),
            &[&run_id],
        )
        .await
        .expect("steal workflow claim");
        assert_eq!(affected, 1, "steal the seeded run claim");
    }

    pub async fn step_names(&self, run_id: &str) -> Vec<String> {
        let conn = self.admin;
        let tables = &self.tables;
        conn.query(
            &format!(
                "SELECT name FROM {} WHERE run_id = $1 ORDER BY ordinal",
                tables.steps
            ),
            &[&run_id],
        )
        .await
        .expect("load workflow step names")
        .into_iter()
        .map(|row| row.get("name"))
        .collect()
    }

    pub async fn step_output(&self, run_id: &str, name: &str) -> serde_json::Value {
        let conn = self.admin;
        let tables = &self.tables;
        conn.query_one(
            &format!(
                "SELECT output FROM {} WHERE run_id = $1 AND name = $2",
                tables.steps
            ),
            &[&run_id, &name],
        )
        .await
        .expect("load workflow step output")
        .get::<_, Option<serde_json::Value>>("output")
        .expect("inline workflow step output")
    }
}
