use super::{transport::Transport, Error, Options};
use std::sync::Arc;
use zeroship_core::{
    app_id::AppId,
    schema_bundle::{EnsureJournal, SchemaBundleOutcome},
    service_assertion::ServiceIssuer,
    service_identity::endpoints,
    service_peers::{service_issuer, ServiceAuth, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        Assignment, ManageRun, ManagementOperation, ManagementReceipt, ManagementStatus, RequestId,
        RestartOptions, RunId, RunOperation, VerifyAssignment, AUDIENCE,
    },
    workflow_jobs::{JobOperation, JobSpec},
    workflow_schedules::{ActivateSchedules, DisableSchedules, RegisterSchedules},
};

/// A runtime-local coordinator client bound to the trusted Control signer.
///
/// The client exchanges placement, management and schedule metadata. Assignment receipts
/// never grant access to customer credentials, journals or payloads.
#[derive(Clone, Debug)]
pub struct ControlCoordinator {
    transport: Transport,
    schedule_publisher: bool,
}

impl ControlCoordinator {
    /// Remote coordinators require verified HTTPS; literal loopback hosts may
    /// use HTTP. Mutations are never retried automatically.
    ///
    /// # Errors
    /// Rejects missing or non-Control signers, ambiguous endpoints and empty bounds.
    pub fn new(url: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, Error> {
        let (issuer, _) = auth.signing_identity().ok_or(Error::Unauthenticated)?;
        let role = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?;
        if issuer.principal() != role.principal() {
            return Err(Error::Unauthenticated);
        }
        let schedule_publisher = issuer == &role;
        Ok(Self {
            transport: Transport::new(
                url,
                auth,
                ServiceIssuer::parse(AUDIENCE).map_err(|_| Error::InvalidConfig)?,
                options,
            )?,
            schedule_publisher,
        })
    }

    /// Ask the manager to bring one creator database's workflow journal to the
    /// version the platform currently carries.
    ///
    /// Control is the party that learns an app deployed, so Control is what
    /// asks; the manager holds the journal artifacts and sends them on. Routing
    /// the artifacts through Control instead would put the workflow domain back
    /// inside the control plane.
    ///
    /// Idempotent at the far end - the manager reads the installed stamp and
    /// installs, upgrades, verifies or refuses - so a deploy may call it every
    /// time and no caller-side "already provisioned" flag exists to go stale.
    ///
    /// # Errors
    /// Refuses failed exchanges and an outcome describing another schema.
    pub async fn ensure_journal(&self, schema: &str) -> Result<SchemaBundleOutcome, Error> {
        let outcome: SchemaBundleOutcome = self
            .transport
            .post(
                endpoints::WORKFLOW_JOURNAL_ENSURE,
                &EnsureJournal {
                    schema: schema.to_owned(),
                },
            )
            .await?;
        if outcome.schema != schema {
            return Err(Error::InvalidResponse);
        }
        Ok(outcome)
    }

    /// Prepare immutable schedule metadata using the Control service signer.
    /// An exact echo acknowledges preparation; it does not activate the deployment.
    ///
    /// # Errors
    /// Refuses instance credentials, failed exchanges and any changed declaration.
    pub async fn register_schedules(
        &self,
        request: &RegisterSchedules,
    ) -> Result<RegisterSchedules, Error> {
        if !self.schedule_publisher {
            return Err(Error::Unauthenticated);
        }
        let receipt: RegisterSchedules = self
            .transport
            .post(endpoints::WORKFLOW_SCHEDULE_REGISTER, request)
            .await?;
        if receipt != *request {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }

    /// Activate a prepared deployment with a stable platform-chosen revision.
    /// Retry the original revision after uncertainty. The receipt acknowledges
    /// durable manager work, not completion by the creator worker.
    ///
    /// # Errors
    /// Refuses instance credentials, failed exchanges and foreign activation receipts.
    pub async fn activate_schedules(&self, request: &ActivateSchedules) -> Result<JobSpec, Error> {
        if !self.schedule_publisher {
            return Err(Error::Unauthenticated);
        }
        let job: JobSpec = self
            .transport
            .post(endpoints::WORKFLOW_SCHEDULE_ACTIVATE, request)
            .await?;
        if job.app_id != request.app_id
            || job.operation
                != (JobOperation::Activate {
                    deployment_id: request.deployment_id.clone(),
                    revision: request.revision,
                })
        {
            return Err(Error::InvalidResponse);
        }
        Ok(job)
    }

    /// Stop future calendar publication under a stable Control revision.
    /// The receipt acknowledges the manager's calendar fence; it does not
    /// acknowledge creator admission policy or an executor's shutdown.
    ///
    /// # Errors
    /// Refuses instance credentials, failed exchanges and changed disable receipts.
    pub async fn disable_schedules(
        &self,
        request: &DisableSchedules,
    ) -> Result<DisableSchedules, Error> {
        if !self.schedule_publisher {
            return Err(Error::Unauthenticated);
        }
        let receipt: DisableSchedules = self
            .transport
            .post(endpoints::WORKFLOW_SCHEDULE_DISABLE, request)
            .await?;
        if receipt != *request {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }

    /// Accept durable lifecycle metadata for delivery through the app's job queue.
    /// An exact retry returns the original command receipt and frozen restart target.
    ///
    /// # Errors
    /// Refuses failed exchanges and receipts for another app or request.
    pub async fn manage(&self, request: &ManageRun) -> Result<ManagementReceipt, Error> {
        let receipt: ManagementReceipt = self
            .transport
            .post(endpoints::WORKFLOW_MANAGE, request)
            .await?;
        if receipt.app_id != request.app_id || receipt.request_id != request.request_id {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }

    /// Ask the manager to apply a lifecycle transition to one run.
    ///
    /// This assembles the `Transition` command in one place instead of at every
    /// call site. It opens no endpoint of its own: the exchange is
    /// `WORKFLOW_MANAGE`, so serving a transition cannot add to the manager's
    /// request rate beyond what `manage` already carries.
    ///
    /// The caller supplies `request_id` because an exact retry returns the
    /// original receipt; minting one here would turn every retry into a new
    /// command. A `None` outcome means the manager accepted the command and has
    /// not applied it yet - poll `management_status` with the same pair.
    ///
    /// # Errors
    /// Refuses failed exchanges and receipts for another app or request.
    pub async fn transition(
        &self,
        request_id: &RequestId,
        app_id: &AppId,
        run_id: &RunId,
        operation: RunOperation,
    ) -> Result<ManagementReceipt, Error> {
        self.manage(&ManageRun {
            request_id: request_id.clone(),
            app_id: app_id.clone(),
            run_id: run_id.clone(),
            command: ManagementOperation::Transition { operation },
        })
        .await
    }

    /// Ask the manager to restart one run.
    ///
    /// This assembles the `Restart` command in one place instead of at every
    /// call site. It opens no endpoint of its own: the exchange is
    /// `WORKFLOW_MANAGE`, so serving a restart cannot add to the manager's
    /// request rate beyond what `manage` already carries.
    ///
    /// The caller supplies `request_id` because an exact retry returns the
    /// original receipt, together with the restart target the manager froze for
    /// it; minting one here would turn every retry into a new command against a
    /// run the first one may already have restarted. A `None` outcome means the
    /// manager accepted the command and has not applied it yet - poll
    /// `management_status` with the same pair.
    ///
    /// # Errors
    /// Refuses failed exchanges and receipts for another app or request.
    pub async fn restart(
        &self,
        request_id: &RequestId,
        app_id: &AppId,
        run_id: &RunId,
        options: RestartOptions,
    ) -> Result<ManagementReceipt, Error> {
        self.manage(&ManageRun {
            request_id: request_id.clone(),
            app_id: app_id.clone(),
            run_id: run_id.clone(),
            command: ManagementOperation::Restart { options },
        })
        .await
    }

    /// # Errors
    /// Refuses failed exchanges and status receipts for another app or request.
    pub async fn management_status(
        &self,
        request: &ManagementStatus,
    ) -> Result<Option<ManagementReceipt>, Error> {
        let receipt: Option<ManagementReceipt> = self
            .transport
            .post(endpoints::WORKFLOW_MANAGEMENT_STATUS, request)
            .await?;
        if receipt.as_ref().is_some_and(|receipt| {
            receipt.app_id != request.app_id || receipt.request_id != request.request_id
        }) {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }

    /// Check current placement without renewing it. The returned deadline is
    /// bounded by both the placement and the enrolled worker's registration.
    ///
    /// # Errors
    /// Refuses failed exchanges and any changed app, worker or assignment revision.
    pub async fn verify_assignment(&self, request: &VerifyAssignment) -> Result<Assignment, Error> {
        let assignment: Assignment = self
            .transport
            .post(endpoints::WORKFLOW_VERIFY_ASSIGNMENT, request)
            .await?;
        if (
            &assignment.app_id,
            &assignment.worker_id,
            assignment.revision,
        ) != (
            &request.app_id,
            &request.worker_id,
            request.assignment_revision,
        ) {
            return Err(Error::InvalidResponse);
        }
        Ok(assignment)
    }
}
