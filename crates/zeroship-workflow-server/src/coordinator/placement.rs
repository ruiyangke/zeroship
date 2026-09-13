use super::{deadline, get, now, revision, scope, timestamp, Coordinator, Error};
use compio_postgres::{Row, Transaction};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_coordination::{
        AssignScope, AssignedScope, Assignment, PublishWakeHint, ReleaseScope, RequestId, Revision,
        UnixMillis, VerifyAssignment, WakeHintReceipt, WorkerId,
    },
};

impl Coordinator {
    /// Verify existing app authority without extending registration or placement.
    /// Draining workers may still settle their durable journal intents.
    ///
    /// # Errors
    /// Rejects missing, released, stale or expired authority and database failures.
    pub async fn verify_assignment(&self, request: &VerifyAssignment) -> Result<Assignment, Error> {
        self.transact(async |tx| {
            scope(tx, &request.app_id, false).await?;
            let (row, _) = bound(
                tx,
                &request.worker_id,
                &request.app_id,
                request.assignment_revision,
            )
            .await?;
            let expires = get::<i64>(&row, "expires_at")?.min(get::<i64>(&row, "worker_expires")?);
            Ok(Assignment {
                app_id: request.app_id.clone(),
                worker_id: request.worker_id.clone(),
                revision: request.assignment_revision,
                expires_at: timestamp(expires)?,
            })
        })
        .await
    }

    /// Trusted Control placement. Retained revisions prevent ABA after release.
    ///
    /// # Errors
    /// Rejects conflicting receipts/revisions, unavailable workers, exhausted
    /// placement capacity and database failures.
    pub async fn assign(&self, request: &AssignScope) -> Result<Assignment, Error> {
        self.transact(async |tx| {
            scope(tx, &request.app_id, true).await?;
            let expected = request.expected_revision.map(Revision::get);
            if let Some(row) = receipt(tx, &request.app_id, &request.request_id).await? {
                if get::<&str>(&row,"operation")? != "assign"
                    || get::<&str>(&row,"worker_id")? != request.worker_id.as_str()
                    || get::<Option<i64>>(&row,"expected_revision")? != expected
                { return Err(Error::Conflict); }
                return Ok(Assignment {
                    app_id: request.app_id.clone(), worker_id: request.worker_id.clone(),
                    revision: revision(get(&row,"result_revision")?)?,
                    expires_at: timestamp(get(&row,"result_expires_at")?)?,
                });
            }
            // Serialize capacity admission across different app scopes.
            let worker = tx.query_opt(
                "SELECT capacity,state,expires_at FROM workflow_coordination.workers WHERE worker_id=$1 FOR UPDATE",
                &[&request.worker_id.as_str()],
            ).await?.ok_or(Error::Denied)?;
            let previous = tx.query_opt(
                "SELECT revision FROM workflow_coordination.assignments WHERE app_id=$1 AND worker_id=$2",
                &[&request.app_id.as_str(), &request.worker_id.as_str()],
            ).await?;
            let previous = previous.as_ref().map(|row| get::<i64>(row,"revision")).transpose()?;
            if previous != expected { return Err(Error::Conflict); }
            let now = now(tx).await?;
            if get::<&str>(&worker,"state")? != "ready" || get::<i64>(&worker,"expires_at")? <= now {
                return Err(Error::Denied);
            }
            let occupied = tx.query_one(
                "SELECT count(*) AS occupied FROM workflow_coordination.assignments
                 WHERE worker_id=$1 AND app_id<>$2 AND NOT released AND expires_at>$3",
                &[&request.worker_id.as_str(), &request.app_id.as_str(), &now],
            ).await?;
            if get::<i64>(&occupied,"occupied")? >= get::<i64>(&worker,"capacity")? {
                return Err(Error::Capacity);
            }
            let rev = previous.unwrap_or(0).checked_add(1).ok_or(Error::Conflict)?;
            let expires = deadline(now,self.options.assignment_ttl)?;
            tx.execute(
                "INSERT INTO workflow_coordination.assignments(app_id,worker_id,revision,expires_at,released,id)
                 VALUES($1,$2,$3,$4,false,$5) ON CONFLICT(app_id,worker_id) DO UPDATE SET
                 revision=$3,expires_at=$4,released=false,wake_revision=NULL,next_due_at=NULL",
                &[&request.app_id.as_str(), &request.worker_id.as_str(), &rev, &expires, &typed_id::generate("wca")],
            ).await?;
            tx.execute(
                "INSERT INTO workflow_coordination.placement_receipts(app_id,request_id,operation,worker_id,expected_revision,result_revision,result_expires_at,id)
                 VALUES($1,$2,'assign',$3,$4,$5,$6,$7)",
                &[&request.app_id.as_str(), &request.request_id.as_str(), &request.worker_id.as_str(), &expected, &rev, &expires, &typed_id::generate("wcp")],
            ).await?;
            Ok(Assignment { app_id: request.app_id.clone(), worker_id: request.worker_id.clone(), revision: revision(rev)?, expires_at: timestamp(expires)? })
        }).await
    }

    /// Registration only reports liveness; renewal checks placement separately.
    ///
    /// # Errors
    /// Rejects stale or foreign placement authority and database failures.
    pub async fn renew(
        &self,
        worker: &WorkerId,
        request: &AssignedScope,
    ) -> Result<Assignment, Error> {
        self.transact(async |tx| {
            scope(tx, &request.app_id, false).await?;
            let (_, now) = bound(tx,worker,&request.app_id,request.assignment_revision).await?;
            let expires = deadline(now,self.options.assignment_ttl)?;
            tx.execute(
                "UPDATE workflow_coordination.assignments SET expires_at=$3 WHERE app_id=$1 AND worker_id=$2",
                &[&request.app_id.as_str(), &worker.as_str(), &expires],
            ).await?;
            Ok(Assignment { app_id: request.app_id.clone(), worker_id: worker.clone(), revision: request.assignment_revision, expires_at: timestamp(expires)? })
        }).await
    }

    /// # Errors
    /// Returns `Unavailable` when placements cannot be read or decoded.
    pub async fn assignments(
        &self,
        worker: &WorkerId,
        after: Option<&AppId>,
    ) -> Result<Vec<Assignment>, Error> {
        let after = after.map(AppId::as_str);
        let limit = i64::try_from(self.options.batch_limit).map_err(|_| Error::Invalid)?;
        self.pool.query(
            "SELECT a.app_id,a.worker_id,a.revision,a.expires_at FROM workflow_coordination.assignments a
             JOIN workflow_coordination.workers w USING(worker_id)
             WHERE a.worker_id=$1 AND ($2::text IS NULL OR a.app_id>$2) AND NOT a.released
             AND a.expires_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint
             AND w.expires_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint
             ORDER BY a.app_id LIMIT $3", &[&worker.as_str(), &after, &limit],
        ).await?.iter().map(|row| Ok(Assignment {
            app_id: AppId::parse(get(row,"app_id")?).map_err(|_| Error::Unavailable)?,
            worker_id: worker.clone(), revision: revision(get(row,"revision")?)?,
            expires_at: timestamp(get(row,"expires_at")?)?,
        })).collect()
    }

    /// # Errors
    /// Rejects foreign/expired placements, conflicting wake revisions and database failures.
    pub async fn publish_wake(
        &self,
        worker: &WorkerId,
        request: &PublishWakeHint,
    ) -> Result<WakeHintReceipt, Error> {
        self.transact(async |tx| {
            scope(tx,&request.app_id,false).await?;
            let (row,_) = bound(tx,worker,&request.app_id,request.assignment_revision).await?;
            let previous: Option<i64> = get(&row,"wake_revision")?;
            let rev = request.revision.get();
            let due = request.next_due_at.map(UnixMillis::get);
            if previous.is_some_and(|old| old>rev)
                || (previous==Some(rev) && get::<Option<i64>>(&row,"next_due_at")? != due)
            { return Err(Error::Conflict); }
            tx.execute(
                "UPDATE workflow_coordination.assignments SET wake_revision=$3,next_due_at=$4 WHERE app_id=$1 AND worker_id=$2",
                &[&request.app_id.as_str(), &worker.as_str(), &rev, &due],
            ).await?;
            Ok(WakeHintReceipt { app_id: request.app_id.clone(), assignment_revision: request.assignment_revision, revision: request.revision })
        }).await
    }

    /// Keep a responsible worker until a host implements durable zero-scale wake
    /// delivery. A persisted hint alone is insufficient to release the last one.
    ///
    /// # Errors
    /// Rejects stale authority, changed receipts, unacknowledged hints, absence
    /// of a responsible peer and database failures.
    pub async fn release(&self, worker: &WorkerId, request: &ReleaseScope) -> Result<(), Error> {
        self.transact(async |tx| {
            scope(tx,&request.app_id,false).await?;
            let expected = request.assignment_revision.get();
            let wake = request.wake_revision.get();
            if let Some(row) = receipt(tx,&request.app_id,&request.request_id).await? {
                if get::<&str>(&row,"operation")? != "release"
                    || get::<&str>(&row,"worker_id")? != worker.as_str()
                    || get::<Option<i64>>(&row,"expected_revision")? != Some(expected)
                    || get::<Option<i64>>(&row,"wake_revision")? != Some(wake)
                { return Err(Error::Conflict); }
                return Ok(());
            }
            let (row,now) = bound(tx,worker,&request.app_id,request.assignment_revision).await?;
            if get::<Option<i64>>(&row,"wake_revision")? != Some(wake) { return Err(Error::Conflict); }
            let backup = tx.query_opt(
                "SELECT a.worker_id FROM workflow_coordination.assignments a JOIN workflow_coordination.workers w USING(worker_id)
                 WHERE a.app_id=$1 AND a.worker_id<>$2 AND NOT a.released AND a.expires_at>$3
                 AND w.state='ready' AND w.expires_at>$3 LIMIT 1",
                &[&request.app_id.as_str(), &worker.as_str(), &now],
            ).await?;
            if backup.is_none() { return Err(Error::Conflict); }
            tx.execute(
                "UPDATE workflow_coordination.assignments SET released=true WHERE app_id=$1 AND worker_id=$2",
                &[&request.app_id.as_str(), &worker.as_str()],
            ).await?;
            tx.execute(
                "INSERT INTO workflow_coordination.placement_receipts(app_id,request_id,operation,worker_id,expected_revision,wake_revision,result_revision,result_expires_at,id)
                 VALUES($1,$2,'release',$3,$4,$5,$4,$6,$7)",
                &[&request.app_id.as_str(), &request.request_id.as_str(), &worker.as_str(), &expected, &wake, &now, &typed_id::generate("wcp")],
            ).await?;
            Ok(())
        }).await
    }
}

async fn receipt(
    tx: &Transaction<'_>,
    app: &AppId,
    request: &RequestId,
) -> Result<Option<Row>, Error> {
    Ok(tx.query_opt(
        "SELECT operation,worker_id,expected_revision,wake_revision,result_revision,result_expires_at
         FROM workflow_coordination.placement_receipts WHERE app_id=$1 AND request_id=$2",
        &[&app.as_str(), &request.as_str()],
    ).await?)
}

/// Caller holds the scope lock. Read the clock after locks so waiting cannot
/// turn stale placement authority into a successful mutation.
pub(super) async fn bound(
    tx: &Transaction<'_>,
    worker: &WorkerId,
    app: &AppId,
    rev: Revision,
) -> Result<(Row, i64), Error> {
    let row = tx.query_opt(
        "SELECT a.revision,a.expires_at,a.released,a.wake_revision,a.next_due_at,w.expires_at AS worker_expires
         FROM workflow_coordination.assignments a JOIN workflow_coordination.workers w USING(worker_id)
         WHERE a.app_id=$1 AND a.worker_id=$2 FOR UPDATE OF a,w",
        &[&app.as_str(), &worker.as_str()],
    ).await?.ok_or(Error::Denied)?;
    let now = now(tx).await?;
    if get::<i64>(&row, "revision")? != rev.get()
        || get::<bool>(&row, "released")?
        || get::<i64>(&row, "expires_at")? <= now
        || get::<i64>(&row, "worker_expires")? <= now
    {
        return Err(Error::Denied);
    }
    Ok((row, now))
}
