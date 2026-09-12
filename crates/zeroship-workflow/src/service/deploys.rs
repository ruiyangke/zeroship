//! Executable snapshots selected and retained by the trusted customer worker.

use super::{
    app::{decode, encode, lock_app},
    store::Transaction,
    AppPolicy, DeployRegistration, WorkflowService,
};
use crate::{validation, WorkflowServiceError};
use zeroship_core::{app_id::AppId, typed_id};

impl WorkflowService {
    /// Install a deployment selected by a trusted embedded host.
    pub async fn activate_deploy(
        &self,
        app: &AppId,
        deploy: &DeployRegistration,
    ) -> Result<(), WorkflowServiceError> {
        let mut tx = self.begin().await?;
        let policy = lock_app(&mut tx, app).await?;
        let now = tx.now().await?;
        install(&mut tx, app, deploy, &policy, now).await?;
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
