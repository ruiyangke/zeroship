//! Executable snapshots and reconciliation against Control's current selection.

use super::{
    app::{decode, encode, lock_app},
    store::Transaction,
    AppPolicy, DeployRegistration, ScheduleRegistration, WorkflowService,
};
use crate::{validation, WorkflowServiceError};
use serde::Deserialize;
use zeroship_core::{app_id::AppId, typed_id};

impl WorkflowService {
    /// Install a deployment selected by a trusted embedded host.
    pub async fn activate_deploy(
        &self,
        app: &AppId,
        deploy: &DeployRegistration,
    ) -> Result<(), WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        if tx.platform_policy.is_some() {
            return Err(WorkflowServiceError::InvalidRequest(
                "platform workflow deployments are selected by Control".into(),
            ));
        }
        let policy = lock_app(&mut tx, app).await?;
        let now = tx.now().await?;
        install(&mut tx, app, deploy, &policy, now).await?;
        tx.commit().await
    }

    /// A notification is a hint to read authority, never a deployment selector.
    pub async fn reconcile_deploy(&self, app: &AppId) -> Result<(), WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        if tx.platform_policy.is_none() {
            return Err(WorkflowServiceError::InvalidRequest(
                "deployment reconciliation requires platform authority".into(),
            ));
        }
        let policy = lock_app(&mut tx, app).await?;
        reconcile_platform(&mut tx, app, &policy).await?;
        tx.commit().await
    }
}

async fn install(
    tx: &mut Transaction,
    app: &AppId,
    deploy: &DeployRegistration,
    policy: &AppPolicy,
    activated_at: i64,
) -> Result<(), WorkflowServiceError> {
    let now = tx.now().await?;
    typed_id::parse_with_prefix(&deploy.id, "dep").map_err(|_| {
        WorkflowServiceError::InvalidRequest("invalid workflow deploy identity".into())
    })?;
    if deploy.hash.len() != 64
        || !deploy
            .hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(WorkflowServiceError::InvalidRequest(
            "invalid workflow deploy hash".into(),
        ));
    }
    for name in &deploy.workflows {
        validation::workflow_name(name)?;
    }
    let table = tx.table("deploys");
    let rows = tx
        .query(
            &format!("SELECT hash,manifest,state FROM {table} WHERE app_id=$1 AND id=$2"),
            &[app.as_str().into(), deploy.id.clone().into()],
        )
        .await?;
    if let Some(existing) = rows.first() {
        let manifest: DeployRegistration = decode(&existing.text("manifest")?)?;
        if manifest != *deploy
            || existing.text("hash")? != deploy.hash
            || existing.text("state")? != "available"
        {
            return Err(WorkflowServiceError::Conflict(
                "workflow deployment is immutable or being deleted".into(),
            ));
        }
    } else {
        tx.execute(&format!("INSERT INTO {table} (app_id,id,hash,manifest,created_at,active,state) VALUES ($1,$2,$3,$4,$5,0,'available')"),
                &[app.as_str().into(),deploy.id.clone().into(),deploy.hash.clone().into(),encode(deploy)?.into(),now.into()]).await?;
    }
    tx.execute(
        &format!("UPDATE {table} SET active=0 WHERE app_id=$1"),
        &[app.as_str().into()],
    )
    .await?;
    tx.execute(
        &format!("UPDATE {table} SET active=1 WHERE app_id=$1 AND id=$2"),
        &[app.as_str().into(), deploy.id.clone().into()],
    )
    .await?;
    super::schedules::reconcile(tx, app, deploy, policy, activated_at).await?;

    Ok(())
}

#[derive(Deserialize)]
struct ManifestWorkflows {
    #[serde(default)]
    workflows: Option<Vec<String>>,
    #[serde(default)]
    schedules: Vec<ScheduleRegistration>,
}

// Call only after lock_app. Its shared platform app fence freezes selection
// and history writers until the workflow transaction commits.
pub(crate) async fn reconcile_platform(
    tx: &mut Transaction,
    app: &AppId,
    policy: &AppPolicy,
) -> Result<(), WorkflowServiceError> {
    if tx.platform_policy.is_none() {
        return Ok(());
    }
    let app_id = app.uuid().to_string();
    let rows = tx
        .query(
            "SELECT deploy_hash,manifest_json FROM zeroship.apps WHERE id=$1::text::uuid",
            &[app_id.clone().into()],
        )
        .await?;
    let current = rows.first().ok_or_else(invalid_authority)?;
    let revision = super::deploy_notifications::revision(tx, app).await?;
    let Some(hash) = current.optional_text("deploy_hash")? else {
        let deploys = tx.table("deploys");
        let schedules = tx.table("schedules");
        tx.execute(
            &format!("UPDATE {deploys} SET active=0 WHERE app_id=$1"),
            &[app.as_str().into()],
        )
        .await?;
        tx.execute(
            &format!("UPDATE {schedules} SET next_at=NULL WHERE app_id=$1"),
            &[app.as_str().into()],
        )
        .await?;
        super::deploy_notifications::acknowledge(tx, app, revision).await?;
        return Ok(());
    };
    if revision.is_none() {
        return Err(invalid_authority());
    }
    let manifest = current
        .optional_text("manifest_json")?
        .ok_or_else(invalid_authority)?;
    let rows = tx.query("SELECT id,manifest_json,CAST(FLOOR(EXTRACT(EPOCH FROM activated_at)*1000) AS BIGINT) AS activated_at FROM zeroship.app_deploys WHERE app_id=$1::text::uuid AND deploy_hash=$2", &[app_id.into(),hash.clone().into()]).await?;
    let history = rows.first().ok_or_else(invalid_authority)?;
    if rows.len() != 1 || history.text("manifest_json")? != manifest {
        return Err(invalid_authority());
    }
    let manifest: ManifestWorkflows =
        serde_json::from_str(&manifest).map_err(|_| invalid_authority())?;
    let names = manifest.workflows.unwrap_or_default();
    let workflows: std::collections::BTreeSet<_> = names.iter().cloned().collect();
    if workflows.len() != names.len() {
        return Err(invalid_authority());
    }
    let deploy = DeployRegistration {
        id: history.text("id")?,
        hash,
        workflows,
        schedules: manifest.schedules,
    };
    install(tx, app, &deploy, policy, history.integer("activated_at")?).await?;
    super::deploy_notifications::acknowledge(tx, app, revision).await
}

fn invalid_authority() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(
        "current workflow deployment authority is missing or invalid".into(),
    )
}
