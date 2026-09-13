use super::{count, deadline, one, revision, rows, timestamp, update, Coordinator, Error};
use crate::{
    clock::Sample,
    models::{assignments, placement_receipts, workers, Placement, PlacementReceipt, Worker},
};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_coordination::{
        AssignScope, AssignedScope, Assignment, PublishWakeHint, ReleaseScope, RequestId, Revision,
        UnixMillis, VerifyAssignment, WakeHintReceipt, WorkerId,
    },
};
use zeroship_data_orm::{
    orm::{Database, Entity},
    value,
};

impl Coordinator {
    /// Verify placement without extending its lifetime.
    ///
    /// # Errors
    /// Rejects missing, released, stale or expired assignments and storage failures.
    pub async fn verify_assignment(&self, request: &VerifyAssignment) -> Result<Assignment, Error> {
        let budget = self.budget();
        self.queue
            .transact_for(budget.clone(), |tx| async move {
                self.scope(&tx, &request.app_id, false).await?;
                let (row, sample, expires) = self
                    .bound(
                        &tx,
                        &request.worker_id,
                        &request.app_id,
                        request.assignment_revision,
                    )
                    .await?;
                budget.cap(sample, expires)?;
                assignment(&row, Some(expires))
            })
            .await
    }

    pub(crate) async fn verify_assignment_in(
        &self,
        tx: &Database,
        request: &VerifyAssignment,
    ) -> Result<Assignment, Error> {
        let (row, _, expires) = self
            .bound(
                tx,
                &request.worker_id,
                &request.app_id,
                request.assignment_revision,
            )
            .await?;
        assignment(&row, Some(expires))
    }

    /// Trusted platform placement preserves receipt identities and revision tombstones.
    ///
    /// # Errors
    /// Rejects conflicting requests, unavailable workers and exhausted capacity.
    pub async fn assign(&self, request: &AssignScope) -> Result<Assignment, Error> {
        let budget = self.budget();
        self.queue.transact_for(budget.clone(), |tx| async move {
            self.scope(&tx, &request.app_id, true).await?;
            let expected = request.expected_revision.map(Revision::get);
            if let Some(receipt) = receipt(&tx, &request.app_id, &request.request_id).await? {
                if receipt.operation != "assign" || receipt.worker_id != request.worker_id.as_str()
                    || receipt.expected_revision != expected {
                    return Err(Error::Conflict);
                }
                return Ok(Assignment { app_id: request.app_id.clone(), worker_id: request.worker_id.clone(),
                    revision: revision(receipt.result_revision)?, expires_at: timestamp(receipt.result_expires_at)? });
            }
            // Scope before worker: the worker row serializes capacity across apps.
            let worker = lock_worker(&tx, &request.worker_id).await?;
            let previous = placement(&tx, &request.app_id, &request.worker_id).await?;
            if previous.as_ref().map(|row| row.revision) != expected { return Err(Error::Conflict); }
            let sample = self.queue.clock.sample().await?;
            if worker.state != "ready" || worker.expires_at <= sample.millis { return Err(Error::Denied); }
            budget.cap(sample, worker.expires_at)?;
            let occupied = count::<assignments::Entity>(&tx, value!({
                "worker_id":request.worker_id.as_str(), "app_id":{"$ne":request.app_id.as_str()},
                "released":false, "expires_at":{"$gt":sample.millis}
            })).await?;
            if occupied >= worker.capacity { return Err(Error::Capacity); }
            let rev = expected.unwrap_or(0).checked_add(1).ok_or(Error::Conflict)?;
            let expires = deadline(sample.millis, self.options.assignment_ttl)?;
            if let Some(previous) = previous {
                update::<assignments::Entity>(&tx, value!({"id":previous.id,"app_id":request.app_id.as_str(),"worker_id":request.worker_id.as_str(),"revision":previous.revision}),
                    value!({"revision":rev,"expires_at":expires,"released":false,"wake_revision":null,"next_due_at":null})).await?;
            } else {
                tx.collection(assignments::Entity::COLLECTION)?.insert(value!({
                    "id":typed_id::generate("wca"),"app_id":request.app_id.as_str(),"worker_id":request.worker_id.as_str(),
                    "revision":rev,"expires_at":expires,"released":false
                })).await?;
            }
            tx.collection(placement_receipts::Entity::COLLECTION)?.insert(value!({
                "id":typed_id::generate("wcp"),"app_id":request.app_id.as_str(),"request_id":request.request_id.as_str(),
                "operation":"assign","worker_id":request.worker_id.as_str(),"expected_revision":expected,
                "result_revision":rev,"result_expires_at":expires
            })).await?;
            budget.cap(self.queue.clock.sample().await?, worker.expires_at.min(expires))?;
            Ok(Assignment { app_id:request.app_id.clone(), worker_id:request.worker_id.clone(), revision:revision(rev)?, expires_at:timestamp(expires)? })
        }).await
    }

    /// # Errors
    /// Rejects foreign, stale or expired placement authority and storage failures.
    pub async fn renew(
        &self,
        worker: &WorkerId,
        request: &AssignedScope,
    ) -> Result<Assignment, Error> {
        let budget = self.budget();
        self.queue.transact_for(budget.clone(), |tx| async move {
            self.scope(&tx, &request.app_id, false).await?;
            let (_, sample, authority_expires) = self.bound(&tx, worker, &request.app_id, request.assignment_revision).await?;
            budget.cap(sample, authority_expires)?;
            let expires = deadline(sample.millis, self.options.assignment_ttl)?;
            update::<assignments::Entity>(&tx, value!({"app_id":request.app_id.as_str(),"worker_id":worker.as_str(),"revision":request.assignment_revision.get(),"released":false}), value!({"expires_at":expires})).await?;
            budget.cap(self.queue.clock.sample().await?, authority_expires)?;
            Ok(Assignment { app_id:request.app_id.clone(), worker_id:worker.clone(), revision:request.assignment_revision, expires_at:timestamp(expires)? })
        }).await
    }

    /// # Errors
    /// Rejects unavailable or invalid stored placement metadata.
    pub async fn assignments(
        &self,
        worker: &WorkerId,
        after: Option<&AppId>,
    ) -> Result<Vec<Assignment>, Error> {
        self.queue
            .transact(|tx| async move {
                let Some(registration) =
                    one::<workers::Entity, Worker>(&tx, value!({"id":worker.as_str()})).await?
                else {
                    return Ok(Vec::new());
                };
                let now = self.queue.clock.now().await?;
                if registration.expires_at <= now {
                    return Ok(Vec::new());
                }
                let mut filter =
                    value!({"worker_id":worker.as_str(),"released":false,"expires_at":{"$gt":now}});
                if let Some(after) = after {
                    filter["app_id"] = value!({"$gt":after.as_str()});
                }
                rows::<assignments::Entity, Placement>(
                    &tx,
                    filter,
                    value!({"app_id":1}),
                    self.options.batch_limit,
                )
                .await?
                .into_iter()
                .map(|row| assignment(&row, None))
                .collect()
            })
            .await
    }

    /// # Errors
    /// Rejects expired assignments and changed or stale wake revisions.
    pub async fn publish_wake(
        &self,
        worker: &WorkerId,
        request: &PublishWakeHint,
    ) -> Result<WakeHintReceipt, Error> {
        let budget = self.budget();
        self.queue.transact_for(budget.clone(), |tx| async move {
            self.scope(&tx, &request.app_id, false).await?;
            let (row, sample, expires) = self.bound(&tx, worker, &request.app_id, request.assignment_revision).await?;
            budget.cap(sample, expires)?;
            let rev = request.revision.get();
            let due = request.next_due_at.map(UnixMillis::get);
            if row.wake_revision.is_some_and(|old| old > rev)
                || (row.wake_revision == Some(rev) && row.next_due_at != due) { return Err(Error::Conflict); }
            update::<assignments::Entity>(&tx, value!({"id":row.id,"app_id":request.app_id.as_str(),"revision":request.assignment_revision.get()}), value!({"wake_revision":rev,"next_due_at":due})).await?;
            budget.cap(self.queue.clock.sample().await?, expires)?;
            Ok(WakeHintReceipt { app_id:request.app_id.clone(),assignment_revision:request.assignment_revision,revision:request.revision })
        }).await
    }

    /// Preserve a responsible peer until manager-owned recovery dispatch replaces wake hints.
    ///
    /// # Errors
    /// Rejects stale authority, conflicting receipts and absence of a responsible peer.
    pub async fn release(&self, worker: &WorkerId, request: &ReleaseScope) -> Result<(), Error> {
        let budget = self.budget();
        self.queue.transact_for(budget.clone(), |tx| async move {
            self.scope(&tx, &request.app_id, false).await?;
            let expected = request.assignment_revision.get();
            let wake = request.wake_revision.get();
            if let Some(receipt) = receipt(&tx, &request.app_id, &request.request_id).await? {
                if receipt.operation != "release" || receipt.worker_id != worker.as_str()
                    || receipt.expected_revision != Some(expected) || receipt.wake_revision != Some(wake) { return Err(Error::Conflict); }
                return Ok(());
            }
            let (row, sample, expires) = self.bound(&tx, worker, &request.app_id, request.assignment_revision).await?;
            budget.cap(sample, expires)?;
            if row.wake_revision != Some(wake) { return Err(Error::Conflict); }
            if !self.has_owner(&tx, &request.app_id, Some(worker), true).await? { return Err(Error::Conflict); }
            update::<assignments::Entity>(&tx, value!({"id":row.id,"app_id":request.app_id.as_str(),"revision":expected,"released":false}), value!({"released":true})).await?;
            tx.collection(placement_receipts::Entity::COLLECTION)?.insert(value!({
                "id":typed_id::generate("wcp"),"app_id":request.app_id.as_str(),"request_id":request.request_id.as_str(),
                "operation":"release","worker_id":worker.as_str(),"expected_revision":expected,"wake_revision":wake,
                "result_revision":expected,"result_expires_at":sample.millis
            })).await?;
            budget.cap(self.queue.clock.sample().await?, expires)?;
            Ok(())
        }).await
    }

    /// Caller holds the common app lock; take the worker lock before reading authority.
    pub(super) async fn bound(
        &self,
        tx: &Database,
        worker: &WorkerId,
        app: &AppId,
        rev: Revision,
    ) -> Result<(Placement, Sample, i64), Error> {
        let registration = lock_worker(tx, worker).await?;
        let row = placement(tx, app, worker).await?.ok_or(Error::Denied)?;
        let sample = self.queue.clock.sample().await?;
        let expires = row.expires_at.min(registration.expires_at);
        if row.revision != rev.get() || row.released || expires <= sample.millis {
            return Err(Error::Denied);
        }
        Ok((row, sample, expires))
    }

    pub(super) async fn has_owner(
        &self,
        tx: &Database,
        app: &AppId,
        exclude: Option<&WorkerId>,
        ready: bool,
    ) -> Result<bool, Error> {
        let mut cursor = None;
        loop {
            let now = self.queue.clock.now().await?;
            let mut filter =
                value!({"app_id":app.as_str(),"released":false,"expires_at":{"$gt":now}});
            if let Some(after) = cursor.as_ref() {
                filter["worker_id"] = value!({"$gt":after});
            }
            let placements = rows::<assignments::Entity, Placement>(
                tx,
                filter,
                value!({"worker_id":1}),
                self.options.batch_limit,
            )
            .await?;
            if placements.is_empty() {
                return Ok(false);
            }
            for row in placements {
                cursor = Some(row.worker_id.clone());
                if exclude.is_some_and(|worker| worker.as_str() == row.worker_id) {
                    continue;
                }
                if let Some(worker) =
                    one::<workers::Entity, Worker>(tx, value!({"id":row.worker_id})).await?
                {
                    let now = self.queue.clock.now().await?;
                    if worker.expires_at > now
                        && row.expires_at > now
                        && (!ready || worker.state == "ready")
                    {
                        return Ok(true);
                    }
                }
            }
        }
    }
}

async fn lock_worker(tx: &Database, worker: &WorkerId) -> Result<Worker, Error> {
    update::<workers::Entity>(
        tx,
        value!({"id":worker.as_str()}),
        value!({"$inc":{"lock_version":0}}),
    )
    .await
    .map_err(|error| {
        if error == Error::Conflict {
            Error::Denied
        } else {
            error
        }
    })?;
    one::<workers::Entity, Worker>(tx, value!({"id":worker.as_str()}))
        .await?
        .ok_or(Error::Denied)
}

async fn placement(
    tx: &Database,
    app: &AppId,
    worker: &WorkerId,
) -> Result<Option<Placement>, Error> {
    one::<assignments::Entity, Placement>(
        tx,
        value!({"app_id":app.as_str(),"worker_id":worker.as_str()}),
    )
    .await
}

async fn receipt(
    tx: &Database,
    app: &AppId,
    request: &RequestId,
) -> Result<Option<PlacementReceipt>, Error> {
    one::<placement_receipts::Entity, PlacementReceipt>(
        tx,
        value!({"app_id":app.as_str(),"request_id":request.as_str()}),
    )
    .await
}

fn assignment(row: &Placement, expires: Option<i64>) -> Result<Assignment, Error> {
    Ok(Assignment {
        app_id: AppId::parse(&row.app_id).map_err(|_| Error::Storage)?,
        worker_id: WorkerId::parse(&row.worker_id).map_err(|_| Error::Storage)?,
        revision: revision(row.revision)?,
        expires_at: timestamp(expires.unwrap_or(row.expires_at))?,
    })
}
