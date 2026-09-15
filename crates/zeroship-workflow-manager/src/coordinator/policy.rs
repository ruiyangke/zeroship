use super::Coordinator;
use crate::{
    policy::{source_deadline, PolicyGrant, PolicySource},
    queue::{bounded, local_deadline},
    Error,
};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use zeroship_core::{workflow_coordination::WorkerId, workflow_policy::PolicyLeaseRequest};

impl Coordinator {
    /// Issue policy for an exact enrolled key under its existing app placement.
    /// The host derives the key thumbprint from the verified assertion and its
    /// callback revalidates that same key, not another active instance key.
    /// Neither registration nor placement is renewed by requesting policy.
    ///
    /// The lease carries the scope's ingress epoch while responsibility is open
    /// or closing. An establishment request commits an open epoch above the
    /// named one under the app lock, after the placement and enrollment checks,
    /// before the grant is issued; a plain refresh never reopens responsibility.
    ///
    /// # Errors
    /// Refuses invalid placement, revoked enrollment, unavailable source authority,
    /// establishment while policy disables admission, an establishment naming an
    /// epoch the manager never issued, and operations whose original authority
    /// expires while awaiting I/O.
    pub async fn policy_lease<'a, F, Fut>(
        &self,
        worker: &WorkerId,
        signing_key_id: &str,
        request: &PolicyLeaseRequest,
        source: &'a dyn PolicySource,
        authorize: F,
    ) -> Result<PolicyGrant<'a>, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        let started = Instant::now();
        let budget = self.budget();
        let scope = &request.scope;
        if signing_key_id.is_empty() {
            return Err(Error::Invalid);
        }
        bounded(budget.clone(), async {
            // Verify the caller's scope before asking the source about an app.
            // Release these locks before source I/O, retaining the original
            // placement deadline through the later locked admission check.
            let placement_expires = self
                .queue
                .transact_for(budget.clone(), |tx| {
                    let authorize = &authorize;
                    let budget = &budget;
                    async move {
                        self.scope(&tx, &scope.app_id, false).await?;
                        let (_, sample, expires) = self
                            .bound(&tx, worker, &scope.app_id, scope.assignment_revision)
                            .await?;
                        let expires_at = local_deadline(sample, expires)?;
                        budget.cap_at(expires_at)?;
                        if &authorize().await? != worker {
                            return Err(Error::Denied);
                        }
                        Ok(expires_at)
                    }
                })
                .await?;
            let observation = source
                .observe(&scope.app_id)
                .await
                .map_err(|_| Error::Unavailable)?;
            if observation.app_id() != &scope.app_id {
                return Err(Error::Unavailable);
            }
            let ceiling = started
                .checked_add(Duration::from_millis(
                    u64::try_from(observation.policy().lease_ms).map_err(|_| Error::Unavailable)?,
                ))
                .ok_or(Error::Unavailable)?;
            let original = ceiling
                .min(placement_expires)
                .min(source_deadline(source, &observation)?);
            budget.cap_at(original)?;
            let (mut expires_at, ingress_epoch) = self
                .queue
                .transact_for(budget.clone(), |tx| {
                    let observation = &observation;
                    let authorize = &authorize;
                    let budget = &budget;
                    async move {
                        self.scope(&tx, &scope.app_id, false).await?;
                        let (_, sample, expires) = self
                            .bound(&tx, worker, &scope.app_id, scope.assignment_revision)
                            .await?;
                        let mut expires_at = original.min(local_deadline(sample, expires)?);
                        expires_at = expires_at.min(source_deadline(source, observation)?);
                        budget.cap_at(expires_at)?;
                        if &authorize().await? != worker {
                            return Err(Error::Denied);
                        }
                        // Responsibility commits with this transaction, before the
                        // lease can reach the worker.
                        let ingress_epoch = Box::pin(crate::recovery::lease_epoch_in(
                            &tx,
                            &scope.app_id,
                            request.establish,
                            request.ingress_used,
                            observation.policy().admission,
                            sample.millis,
                        ))
                        .await?;
                        expires_at = expires_at.min(source_deadline(source, observation)?);
                        budget.cap_at(expires_at)?;
                        Ok((expires_at, ingress_epoch))
                    }
                })
                .await?;
            // Settlement and enrollment queries consume the same original wait
            // budget. A concurrent extension cannot rebase this issuing attempt.
            if &authorize().await? != worker {
                return Err(Error::Denied);
            }
            expires_at = expires_at.min(source_deadline(source, &observation)?);
            budget.cap_at(expires_at)?;
            Ok(PolicyGrant::new(
                observation,
                source,
                worker.clone(),
                signing_key_id.to_owned(),
                scope,
                ingress_epoch,
                expires_at,
            ))
        })
        .await?
    }
}
