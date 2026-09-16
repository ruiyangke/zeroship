//! Placement admits an app onto a worker only within Control's zone and
//! enrollment eligibility. Spare capacity is never authority.
//!
//! Every admission takes the app lock, then the worker lock, then reads the
//! eligibility facts, and reads them again before commit. It admits only when
//! the app is not deleted, the app's zone equals the instance's zone,
//! the instance is live, the registration is ready and unexpired, and the
//! worker has spare capacity. Archived apps stay placeable so maintenance jobs
//! can drain them; policy still refuses their admission, dispatch and ingress.

use super::{count, deadline, one, revision, rows, timestamp, update, Coordinator, Error};
use crate::{
    clock::Sample,
    eligibility::ZoneId,
    models::{assignments, placement_receipts, workers, Placement, PlacementReceipt, Worker},
};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_coordination::{
        AssignedScope, Assignment, ReleaseReason, ReleaseScope, RequestId, Revision,
        VerifyAssignment, WorkerId,
    },
};
use zeroship_data_orm::{
    orm::{Database, Entity},
    value,
};

/// What manager-selected placement found for an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placed {
    /// The app was placed on a selected eligible worker.
    Assigned(Assignment),
    /// A ready, eligible worker already owns the app.
    Owned,
    /// No eligible worker with spare capacity admitted the app.
    Unplaced(ZoneId),
    /// Control has no live app to place: it is unknown or deleted.
    Ineligible,
}

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

    /// Manager-selected placement. The manager chooses among ready workers
    /// registered in the app's zone and admits the first that passes the
    /// eligibility predicate. A refused pair is never offered again.
    ///
    /// # Errors
    /// Reports unavailable eligibility or platform storage.
    pub async fn place(&self, app: &AppId) -> Result<Placed, Error> {
        let facts = match self.eligibility.app(app).await? {
            Some(facts) if !facts.deleted => facts,
            _ => return Ok(Placed::Ineligible),
        };
        let mut after = None;
        loop {
            let page = self.candidates(app, &facts.zone, after.as_ref()).await?;
            let Some(last) = page.last().cloned() else {
                return Ok(Placed::Unplaced(facts.zone));
            };
            for worker in &page {
                match self.admit(app, worker).await {
                    Ok(Some(assignment)) => return Ok(Placed::Assigned(assignment)),
                    Ok(None) => return Ok(Placed::Owned),
                    // The facts, the registration or the capacity changed
                    // after selection; the next candidate is still eligible.
                    Err(Error::Denied | Error::Capacity | Error::Conflict) => {}
                    Err(error) => return Err(error),
                }
            }
            after = Some(last);
        }
    }

    /// Whether a ready worker whose enrollment and zone still match owns the app.
    ///
    /// # Errors
    /// Reports unavailable eligibility or platform storage.
    pub async fn owned(&self, app: &AppId) -> Result<bool, Error> {
        self.queue
            .transact(|tx| async move { self.has_owner(&tx, app, true).await })
            .await
    }

    /// Ready registrations in one zone, excluding workers that refused the app.
    async fn candidates(
        &self,
        app: &AppId,
        zone: &ZoneId,
        after: Option<&WorkerId>,
    ) -> Result<Vec<WorkerId>, Error> {
        self.queue
            .transact(|tx| async move {
                let now = self.queue.clock.now().await?;
                let worker = tx.entity::<workers::Entity>()?.alias("worker")?;
                let refusal = tx.entity::<assignments::Entity>()?.alias("refusal")?;
                let mut filter = worker
                    .column(workers::execution_zone_id)
                    .eq(Some(zone.as_str()))?
                    .and(worker.column(workers::state).eq("ready")?)
                    .and(worker.column(workers::expires_at).gt(now)?)
                    .and(refusal.column(assignments::id).is_null());
                if let Some(after) = after {
                    filter = filter.and(worker.column(workers::id).gt(after.as_str())?);
                }
                tx.from(&worker)
                    .left_join(
                        &refusal,
                        refusal
                            .column(assignments::worker_id)
                            .eq(worker.column(workers::id))?
                            .and(refusal.column(assignments::app_id).eq(app.as_str())?)
                            .and(refusal.column(assignments::refused).eq(true)?),
                    )?
                    .filter(filter)
                    .order_by(worker.column(workers::id).asc())
                    .select(worker.column(workers::id).select::<String>())?
                    .limit(i64::try_from(self.options.batch_limit).map_err(|_| Error::Invalid)?)?
                    .all()
                    .await?
                    .into_iter()
                    .map(|id| WorkerId::parse(&id).map_err(|_| Error::Storage))
                    .collect()
            })
            .await
    }

    /// Admit one selected pair. The manager places only an app that has no
    /// ready eligible owner when it holds the app lock.
    async fn admit(&self, app: &AppId, worker: &WorkerId) -> Result<Option<Assignment>, Error> {
        let budget = self.budget();
        let request_id = RequestId::mint();
        self.queue.transact_for(budget.clone(), |tx| {
            let request_id = &request_id;
            let budget = &budget;
            async move {
            self.scope(&tx, app, true).await?;
            if self.has_owner(&tx, app, true).await? { return Ok(None); }
            // Scope before worker: the worker row serializes capacity across apps.
            let registration = lock_worker(&tx, worker).await?;
            let previous = placement(&tx, app, worker).await?;
            if previous.as_ref().is_some_and(|row| row.refused) { return Err(Error::Denied); }
            // Read the facts in force after every lock wait, not at selection.
            self.eligible(app, worker, &registration).await?;
            let sample = self.queue.clock.sample().await?;
            if registration.state != "ready" || registration.expires_at <= sample.millis { return Err(Error::Denied); }
            budget.cap(sample, registration.expires_at)?;
            let occupied = count::<assignments::Entity>(&tx,
                assignments::worker_id.eq(worker.as_str())?
                    .and(assignments::app_id.ne(app.as_str())?)
                    .and(assignments::released.eq(false)?)
                    .and(assignments::expires_at.gt(sample.millis)?),
            ).await?;
            if occupied >= registration.capacity { return Err(Error::Capacity); }
            let prior = previous.as_ref().map(|row| row.revision);
            let rev = prior.unwrap_or(0).checked_add(1).ok_or(Error::Conflict)?;
            let expires = deadline(sample.millis, self.options.assignment_ttl)?;
            if let Some(previous) = previous {
                update::<assignments::Entity>(&tx, value!({"id":previous.id,"app_id":app.as_str(),"worker_id":worker.as_str(),"revision":previous.revision}),
                    value!({"revision":rev,"expires_at":expires,"released":false})).await?;
            } else {
                tx.collection(assignments::Entity::COLLECTION)?.insert(value!({
                    "id":typed_id::generate("wca"),"app_id":app.as_str(),"worker_id":worker.as_str(),
                    "revision":rev,"expires_at":expires,"released":false,"refused":false
                })).await?;
            }
            tx.collection(placement_receipts::Entity::COLLECTION)?.insert(value!({
                "id":typed_id::generate("wcp"),"app_id":app.as_str(),"request_id":request_id.as_str(),
                "operation":"assign","worker_id":worker.as_str(),"expected_revision":prior,
                "result_revision":rev,"result_expires_at":expires
            })).await?;
            // Again before commit: a revocation committed between the two reads
            // refuses this placement. One committed after this read is caught by
            // the next renewal, ownership or delivery check.
            self.eligible(app, worker, &registration).await?;
            budget.cap(self.queue.clock.sample().await?, registration.expires_at.min(expires))?;
            Ok(Some(Assignment { app_id:app.clone(), worker_id:worker.clone(), revision:revision(rev)?, expires_at:timestamp(expires)? }))
        }}).await
    }

    /// The placement predicate's Control facts: a live app, an active instance,
    /// equal zones, and a registration recorded under that same zone.
    async fn eligible(
        &self,
        app: &AppId,
        worker: &WorkerId,
        registration: &Worker,
    ) -> Result<ZoneId, Error> {
        let facts = self.eligibility.app(app).await?.ok_or(Error::Denied)?;
        if facts.deleted {
            return Err(Error::Denied);
        }
        let zone = self.enrolled_zone(worker).await?;
        if zone != facts.zone || registration.execution_zone_id.as_deref() != Some(zone.as_str()) {
            return Err(Error::Denied);
        }
        Ok(zone)
    }

    /// Renewal rechecks the eligibility predicate under its locks, so an
    /// instance whose enrollment was revoked, or an app that was deleted,
    /// cannot extend its placement.
    ///
    /// # Errors
    /// Rejects foreign, stale, expired or ineligible placement authority and
    /// storage failures.
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
            let registration = one::<workers::Entity, Worker>(&tx, workers::id.eq(worker.as_str())?).await?.ok_or(Error::Denied)?;
            self.eligible(&request.app_id, worker, &registration).await?;
            let expires = deadline(sample.millis, self.options.assignment_ttl)?;
            update::<assignments::Entity>(&tx, value!({"app_id":request.app_id.as_str(),"worker_id":worker.as_str(),"revision":request.assignment_revision.get(),"released":false}), value!({"expires_at":expires})).await?;
            self.eligible(&request.app_id, worker, &registration).await?;
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
                    one::<workers::Entity, Worker>(&tx, workers::id.eq(worker.as_str())?).await?
                else {
                    return Ok(Vec::new());
                };
                let now = self.queue.clock.now().await?;
                if registration.expires_at <= now {
                    return Ok(Vec::new());
                }
                let mut filter = assignments::worker_id
                    .eq(worker.as_str())?
                    .and(assignments::released.eq(false)?)
                    .and(assignments::expires_at.gt(now)?);
                if let Some(after) = after {
                    filter = filter.and(assignments::app_id.gt(after.as_str())?);
                }
                rows::<assignments::Entity, Placement>(
                    &tx,
                    filter,
                    [assignments::app_id.asc()],
                    self.options.batch_limit,
                )
                .await?
                .into_iter()
                .map(|row| assignment(&row, None))
                .collect()
            })
            .await
    }

    /// A worker gives up one of its own placements. Release needs no peer
    /// owner and no hint: recovery responsibility stays with the manager and
    /// the placement lane places the app again while it has due work. A
    /// `refused` release tombstones the app and instance pair, so the manager
    /// never offers it again; a restarted process is a new instance.
    ///
    /// # Errors
    /// Rejects foreign or stale placements and conflicting receipts.
    pub async fn release(&self, worker: &WorkerId, request: &ReleaseScope) -> Result<(), Error> {
        let budget = self.budget();
        let reason = release_reason(request.reason);
        self.queue.transact_for(budget, |tx| async move {
            self.scope(&tx, &request.app_id, false).await?;
            let expected = request.assignment_revision.get();
            if let Some(receipt) = receipt(&tx, &request.app_id, &request.request_id).await? {
                if receipt.operation != "release" || receipt.worker_id != worker.as_str()
                    || receipt.expected_revision != Some(expected) || receipt.reason.as_deref() != Some(reason) { return Err(Error::Conflict); }
                return Ok(());
            }
            lock_worker(&tx, worker).await?;
            let row = placement(&tx, &request.app_id, worker).await?.ok_or(Error::Denied)?;
            if row.revision != expected { return Err(Error::Denied); }
            if row.released { return Err(Error::Conflict); }
            let sample = self.queue.clock.sample().await?;
            update::<assignments::Entity>(&tx, value!({"id":row.id,"app_id":request.app_id.as_str(),"revision":expected,"released":false}),
                value!({"released":true,"refused":request.reason == ReleaseReason::Refused})).await?;
            tx.collection(placement_receipts::Entity::COLLECTION)?.insert(value!({
                "id":typed_id::generate("wcp"),"app_id":request.app_id.as_str(),"request_id":request.request_id.as_str(),
                "operation":"release","worker_id":worker.as_str(),"expected_revision":expected,"reason":reason,
                "result_revision":expected,"result_expires_at":sample.millis
            })).await?;
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

    /// A live placement on a live worker Control still considers live and whose
    /// zone is the app's zone. A deleted or unknown app has no owner.
    pub(crate) async fn has_owner(
        &self,
        tx: &Database,
        app: &AppId,
        ready: bool,
    ) -> Result<bool, Error> {
        let facts = match self.eligibility.app(app).await? {
            Some(facts) if !facts.deleted => facts,
            _ => return Ok(false),
        };
        let mut cursor = None;
        loop {
            let now = self.queue.clock.now().await?;
            let mut filter = assignments::app_id
                .eq(app.as_str())?
                .and(assignments::released.eq(false)?)
                .and(assignments::expires_at.gt(now)?);
            if let Some(after) = cursor.as_deref() {
                filter = filter.and(assignments::worker_id.gt(after)?);
            }
            let placements = rows::<assignments::Entity, Placement>(
                tx,
                filter,
                [assignments::worker_id.asc()],
                self.options.batch_limit,
            )
            .await?;
            if placements.is_empty() {
                return Ok(false);
            }
            for row in placements {
                cursor = Some(row.worker_id.clone());
                let Some(worker) =
                    one::<workers::Entity, Worker>(tx, workers::id.eq(row.worker_id.as_str())?)
                        .await?
                else {
                    continue;
                };
                let now = self.queue.clock.now().await?;
                if worker.expires_at <= now
                    || row.expires_at <= now
                    || (ready && worker.state != "ready")
                    || worker.execution_zone_id.as_deref() != Some(facts.zone.as_str())
                {
                    continue;
                }
                let id = WorkerId::parse(&worker.id).map_err(|_| Error::Storage)?;
                if matches!(self.eligibility.worker(&id).await?, Some(current) if current.active && current.zone == facts.zone)
                {
                    return Ok(true);
                }
            }
        }
    }
}

const fn release_reason(reason: ReleaseReason) -> &'static str {
    match reason {
        ReleaseReason::Relinquished => "relinquished",
        ReleaseReason::Refused => "refused",
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
    one::<workers::Entity, Worker>(tx, workers::id.eq(worker.as_str())?)
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
        assignments::app_id
            .eq(app.as_str())?
            .and(assignments::worker_id.eq(worker.as_str())?),
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
        placement_receipts::app_id
            .eq(app.as_str())?
            .and(placement_receipts::request_id.eq(request.as_str())?),
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
