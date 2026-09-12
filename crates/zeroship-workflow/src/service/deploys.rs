//! Executable snapshots selected and retained by the trusted customer worker.

use super::{
    app::{decode, encode, lock_app},
    snapshots::content_hash,
    DeployRegistration, ExecutableSnapshot, WorkflowService,
};
use crate::{validation, WorkflowServiceError};
use zeroship_core::{app_id::AppId, typed_id};

impl WorkflowService {
    /// Resolve a retained deployment by the identity chosen by its host.
    ///
    /// # Errors
    /// Rejects an unbound app and unavailable journal storage.
    pub async fn deployment_by_hash(
        &self,
        app: &AppId,
        hash: &str,
    ) -> Result<Option<DeployRegistration>, WorkflowServiceError> {
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let rows = tx
            .query(
                &format!(
                    "SELECT manifest FROM {} WHERE app_id=$1 AND hash=$2",
                    tx.table("deploys")
                ),
                &[app.as_str().into(), hash.into()],
            )
            .await?;
        let registration = rows
            .first()
            .map(|row| decode(&row.text("manifest")?))
            .transpose()?;
        tx.commit().await?;
        Ok(registration)
    }

    /// Retain executable bytes before selecting a deployment for new work.
    /// The host serializes its desired-deployment updates. Failed uploads leave
    /// a staging record and cannot replace the currently active deployment.
    ///
    /// # Errors
    /// Rejects invalid or conflicting deployments and unavailable storage.
    pub async fn activate_deploy(
        &self,
        app: &AppId,
        deploy: &DeployRegistration,
        image: &ExecutableSnapshot,
    ) -> Result<(), WorkflowServiceError> {
        self.retain_deploy(app, deploy, image).await?;
        let mut tx = self.begin().await?;
        let policy = lock_app(&mut tx, app).await?;
        let table = tx.table("deploys");
        let available = tx
            .query(
                &format!("SELECT id FROM {table} WHERE app_id=$1 AND id=$2 AND state='available'"),
                &[app.as_str().into(), deploy.id.clone().into()],
            )
            .await?;
        if available.is_empty() {
            return Err(WorkflowServiceError::Unavailable(
                "workflow executable snapshot is unavailable".into(),
            ));
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
        let now = tx.now().await?;
        super::schedules::reconcile(&mut tx, app, deploy, &policy, now).await?;
        tx.commit().await
    }

    /// Retain or repair immutable executable bytes without changing which
    /// deployment new runs and schedules select.
    ///
    /// # Errors
    /// Rejects conflicting deployment contents and unavailable storage.
    #[expect(
        clippy::future_not_send,
        reason = "deployment I/O runs on its owning compio thread"
    )]
    pub async fn retain_deploy(
        &self,
        app: &AppId,
        deploy: &DeployRegistration,
        image: &ExecutableSnapshot,
    ) -> Result<(), WorkflowServiceError> {
        validate(deploy)?;
        let snapshots = self.snapshots.as_ref().ok_or_else(|| {
            WorkflowServiceError::Unavailable("workflow snapshot storage is not bound".into())
        })?;
        let bytes = snapshots.encode(image)?;
        let hash = content_hash(&bytes);
        let size = i64::try_from(bytes.len()).map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let hold_generation =
            super::deployment_retention::admission_generation(&tx, app, &deploy.id, &deploy.hash)
                .await?;
        let now = tx.now().await?;
        let table = tx.table("deploys");
        let rows = tx
            .query(
                &format!("SELECT * FROM {table} WHERE app_id=$1 AND id=$2"),
                &[app.as_str().into(), deploy.id.clone().into()],
            )
            .await?;
        if let Some(existing) = rows.first() {
            check_existing(existing, deploy, &hash, size, hold_generation.is_some())?;
        } else {
            tx.execute(&format!("INSERT INTO {table} (app_id,id,hash,manifest,created_at,active,state,snapshot_hash,snapshot_size,snapshot_epoch) VALUES ($1,$2,$3,$4,$5,0,'staging',$6,$7,0)"),
                &[app.as_str().into(),deploy.id.clone().into(),deploy.hash.clone().into(),encode(deploy)?.into(),now.into(),hash.clone().into(),size.into()]).await?;
        }
        tx.commit().await?;
        // Object I/O cannot hold the app lock and block execution heartbeats.
        snapshots.put(app, &deploy.id, bytes, &hash).await?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        if super::deployment_retention::admission_generation(&tx, app, &deploy.id, &deploy.hash)
            .await?
            != hold_generation
        {
            return Err(conflict());
        }
        let rows = tx
            .query(
                &format!("SELECT * FROM {table} WHERE app_id=$1 AND id=$2"),
                &[app.as_str().into(), deploy.id.clone().into()],
            )
            .await?;
        let existing = rows.first().ok_or_else(conflict)?;
        check_existing(existing, deploy, &hash, size, hold_generation.is_some())?;
        let epoch = existing
            .integer("snapshot_epoch")?
            .checked_add(1)
            .ok_or_else(|| {
                WorkflowServiceError::ResourceExhausted("workflow snapshot epoch exhausted".into())
            })?;
        tx.execute(
            &format!(
                "UPDATE {table} SET state='available',snapshot_epoch=$3 WHERE app_id=$1 AND id=$2"
            ),
            &[app.as_str().into(), deploy.id.clone().into(), epoch.into()],
        )
        .await?;
        tx.commit().await
    }
}

fn check_existing(
    row: &super::store::Row,
    deploy: &DeployRegistration,
    hash: &str,
    size: i64,
    held: bool,
) -> Result<(), WorkflowServiceError> {
    let manifest: DeployRegistration = decode(&row.text("manifest")?)?;
    if manifest != *deploy
        || row.text("hash")? != deploy.hash
        || row.text("snapshot_hash")? != hash
        || row.integer("snapshot_size")? != size
        || !(matches!(
            row.text("state")?.as_str(),
            "staging" | "available" | "unavailable"
        ) || (held && row.text("state")? == "retiring"))
    {
        return Err(conflict());
    }
    Ok(())
}
fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow deployment is immutable or being deleted".into())
}
fn validate(deploy: &DeployRegistration) -> Result<(), WorkflowServiceError> {
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
    Ok(())
}
