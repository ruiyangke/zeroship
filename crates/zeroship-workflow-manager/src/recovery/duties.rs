use super::DutyKind;
use crate::{models::recovery_duties, queue, Error};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_jobs::{JobId, JobOutcome, JobSpec},
};
use zeroship_data_orm::orm::{Database, FromRow, Insertable};

#[derive(FromRow)]
#[orm(entity = recovery_duties)]
pub(super) struct Duty {
    pub id: String,
    pub app_id: String,
    pub kind: String,
    pub next_due_at: i64,
    pub pending_job_id: Option<String>,
}

#[derive(Insertable)]
#[orm(entity = recovery_duties)]
struct NewDuty<'a> {
    id: String,
    app_id: &'a str,
    kind: &'a str,
    next_due_at: i64,
    pending_job_id: Option<&'a str>,
}

impl Duty {
    pub fn validate(&self, app: &AppId, kind: DutyKind) -> Result<(), Error> {
        typed_id::parse_with_prefix(&self.id, "wrd").map_err(|_| Error::Storage)?;
        if self.app_id != app.as_str() || self.kind != kind.as_str() || self.next_due_at < 0 {
            return Err(Error::Storage);
        }
        if let Some(pending) = &self.pending_job_id {
            JobId::parse(pending).map_err(|_| Error::Storage)?;
        }
        Ok(())
    }

    pub async fn pending(
        &self,
        tx: &Database,
        app: &AppId,
        kind: DutyKind,
    ) -> Result<Option<JobSpec>, Error> {
        self.validate(app, kind)?;
        let Some(pending) = &self.pending_job_id else {
            return Ok(None);
        };
        let job = queue::load(tx, app, pending).await?.ok_or(Error::Storage)?;
        let spec = job.spec()?;
        if spec.app_id != *app || spec.id.as_str() != pending || spec.operation != kind.operation()
        {
            return Err(Error::Storage);
        }
        match job.state.as_str() {
            "ready" | "leased" => Ok(Some(spec)),
            "settled" => {
                let outcome: JobOutcome =
                    serde_json::from_str(job.outcome.as_deref().ok_or(Error::Storage)?)
                        .map_err(|_| Error::Storage)?;
                let digest = job.settlement_digest.as_deref().ok_or(Error::Storage)?;
                if !outcome.valid_for(&spec.operation)
                    || digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(Error::Storage);
                }
                Ok(None)
            }
            _ => Err(Error::Storage),
        }
    }
}

pub(super) async fn load(
    tx: &Database,
    app: &AppId,
    kind: DutyKind,
) -> Result<Option<Duty>, Error> {
    let duty = tx
        .entity::<recovery_duties::Entity>()?
        .query()
        .filter(
            recovery_duties::app_id
                .eq(app.as_str())?
                .and(recovery_duties::kind.eq(kind.as_str())?),
        )
        .first::<Duty>()
        .await?;
    if let Some(duty) = &duty {
        duty.validate(app, kind)?;
    }
    Ok(duty)
}

pub(super) async fn validate_pair(tx: &Database, app: &AppId) -> Result<(), Error> {
    let duties = tx
        .entity::<recovery_duties::Entity>()?
        .query()
        .filter(recovery_duties::app_id.eq(app.as_str())?)
        .limit(3)?
        .all::<Duty>()
        .await?;
    if duties.len() != 2 {
        return Err(Error::Storage);
    }
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        let duty = duties
            .iter()
            .find(|duty| duty.kind == kind.as_str())
            .ok_or(Error::Storage)?;
        let _ = duty.pending(tx, app, kind).await?;
    }
    Ok(())
}

pub(super) async fn create_pair(tx: &Database, app: &AppId, now: i64) -> Result<(), Error> {
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        tx.entity::<recovery_duties::Entity>()?
            .insert::<_, Duty>(NewDuty {
                id: typed_id::generate("wrd"),
                app_id: app.as_str(),
                kind: kind.as_str(),
                next_due_at: now,
                pending_job_id: None,
            })
            .await?;
    }
    Ok(())
}
