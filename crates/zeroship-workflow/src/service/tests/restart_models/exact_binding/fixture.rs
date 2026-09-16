use super::*;
use crate::{
    operations::RestartedRun,
    service::{
        control::{Preparation, Rejection},
        store::Row,
    },
};
use std::collections::BTreeMap;
use zeroship_core::workflow_jobs::{JobOperation, JobSpec};
use zeroship_data_orm::{orm::Operation, value, Value};

pub(super) struct Fixture {
    pub service: WorkflowService,
    pub journal: WorkflowService,
    pub owner: AppId,
    pub foreign: AppId,
    pub original: DeployRegistration,
    pub replacement: DeployRegistration,
    pub run: String,
    pub deployments: Deployments,
}

pub(super) fn ready<T>(prepared: Preparation<T>) -> Result<T, WorkflowServiceError> {
    match prepared {
        Preparation::Ready(value) => Ok(value),
        Preparation::Rejected(reason) => Err(match reason {
            Rejection::NotFound => WorkflowServiceError::NotFound("workflow run".into()),
            Rejection::Conflict(message) => WorkflowServiceError::Conflict(message),
            Rejection::Invalid(message) => WorkflowServiceError::InvalidRequest(message),
            Rejection::Denied => WorkflowServiceError::PermissionDenied,
        }),
    }
}

impl Fixture {
    pub async fn new(store: Rc<OrmStore>) -> Self {
        let (service, owner, foreign, deployments) = registered_service(store.clone()).await;
        let mut tx = service.begin().await.unwrap();
        let original = app::active_deploy(&mut tx, &owner).await.unwrap();
        tx.commit().await.unwrap();
        let run = service
            .fixture_app(owner.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap()
            .id;
        let replacement = deployments.deploy(&owner).await;
        service.activate_deploy(&owner, &replacement).await.unwrap();
        // This journal has no artifact source or deployment-hold client.
        let journal = WorkflowService::open(store, service.policies.clone())
            .await
            .unwrap();
        Self {
            service,
            journal,
            owner,
            foreign,
            original,
            replacement,
            run,
            deployments,
        }
    }

    pub fn scope(&self) -> AppWorkflows {
        self.journal.fixture_app(self.owner.clone())
    }

    pub async fn active(&self) -> DeployRegistration {
        let mut tx = self.journal.begin().await.unwrap();
        let active = app::active_deploy(&mut tx, &self.owner).await.unwrap();
        tx.commit().await.unwrap();
        active
    }

    pub async fn apply_exact(
        &self,
        expected: &DeployRegistration,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        let (scope, authority) = attempt(&self.scope());
        authority
            .run(async {
                let mut tx = scope.service.begin().await?;
                app::lock_app(&mut tx, &self.owner).await?;
                let now = tx.now().await?;
                let draft = ready(
                    restart::prepare_draft(
                        &mut tx,
                        &self.owner,
                        &self.run,
                        &RestartOptions::default(),
                        &authority.policy,
                        now,
                    )
                    .await?,
                )?;
                let plan = ready(draft.bind_exact(expected).await?)?;
                let result = plan.apply().await?;
                tx.commit().await?;
                Ok(result)
            })
            .await
    }

    pub async fn assert_generation_in(&self, tx: &Transaction, generation: i64, deployment: &str) {
        let head = journal_rows(
            tx,
            "runs",
            json!({"app_id":self.owner.as_str(),"id":self.run}),
        )
        .await;
        assert_eq!(head.len(), 1);
        assert_eq!(head[0].integer("generation").unwrap(), generation);
        assert_eq!(head[0].text("deploy_id").unwrap(), deployment);
        let history = journal_rows(
            tx,
            "generations",
            json!({"app_id":self.owner.as_str(),"run_id":self.run,"generation":generation}),
        )
        .await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text("deploy_id").unwrap(), deployment);
        let publications = journal_rows(
            tx,
            "job_publications",
            json!({"app_id":self.owner.as_str(),"run_id":self.run,"generation":generation}),
        )
        .await;
        assert_eq!(publications.len(), 1);
        let job: JobSpec =
            serde_json::from_str(&publications[0].text("specification").unwrap()).unwrap();
        assert_eq!(job.app_id, self.owner);
        assert!(
            matches!(job.operation, JobOperation::Advance {deployment_id, run_id, generation: actual, ..}
            if deployment_id.as_str() == deployment && run_id.as_str() == self.run && i64::from(actual) == generation)
        );
    }

    pub async fn assert_generation(&self, generation: i64, deployment: &str) {
        let tx = self.journal.begin().await.unwrap();
        self.assert_generation_in(&tx, generation, deployment).await;
        tx.commit().await.unwrap();
    }

    pub async fn row(&self, table: &str, filter: serde_json::Value) -> Row {
        let tx = self.journal.begin().await.unwrap();
        let mut rows = journal_rows(&tx, table, filter).await;
        assert_eq!(rows.len(), 1, "fixture must select one {table} row");
        tx.commit().await.unwrap();
        rows.pop().unwrap()
    }

    pub async fn patch(&self, table: &str, id: &str, changes: Value) {
        let tx = self.journal.begin().await.unwrap();
        tx.database()
            .collection(&format!("__zeroship_workflow_{table}"))
            .unwrap()
            .execute(Operation::Update {
                filter: value!({"id":id}),
                patch: changes.clone(),
                many: false,
            })
            .await
            .unwrap();
        let rows = journal_rows(&tx, table, json!({"id":id})).await;
        assert_eq!(rows.len(), 1);
        for (field, expected) in changes.as_object().unwrap() {
            assert_eq!(rows[0].0.get(field), Some(expected), "{table}.{field}");
        }
        tx.commit().await.unwrap();
    }

    pub async fn restore(&self, table: &str, mut row: Value) {
        let id = row.as_object_mut().unwrap().shift_remove("id").unwrap();
        self.patch(table, id.as_str().unwrap(), row).await;
    }

    pub async fn snapshot(&self) -> BTreeMap<&'static str, Vec<Value>> {
        let tx = self.journal.begin().await.unwrap();
        let mut state = BTreeMap::new();
        for table in [
            "runs",
            "generations",
            "tasks",
            "steps",
            "waits",
            "subscriptions",
            "signals",
            "payload_refs",
            "job_publications",
            "outbox",
            "requests",
            "management_receipts",
            "deploys",
            "deployment_holds",
        ] {
            state.insert(
                table,
                journal_rows(&tx, table, json!({}))
                    .await
                    .into_iter()
                    .map(|row| row.0)
                    .collect(),
            );
        }
        tx.commit().await.unwrap();
        state
    }
}
