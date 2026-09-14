use super::{transport::Transport, Error, Options};
use std::sync::Arc;
use zeroship_core::{
    app_id::AppId,
    service_assertion::ServiceIssuer,
    service_identity::endpoints,
    service_peers::{service_issuer, ServiceAuth, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        AssignScope, Assignment, ManageRun, ManagementReceipt, ManagementStatus, RegisteredWorker,
        Revision, ScopePage, VerifyAssignment, WorkerPage, WorkerState, AUDIENCE,
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

    /// # Errors
    /// Refuses failed exchanges, non-ready workers and unordered or repeated entries.
    pub async fn ready_workers(
        &self,
        request: &WorkerPage,
    ) -> Result<Vec<RegisteredWorker>, Error> {
        let workers: Vec<RegisteredWorker> = self
            .transport
            .post(endpoints::WORKFLOW_WORKERS, request)
            .await?;
        let mut previous = request.after.as_ref();
        for worker in &workers {
            if worker.state != WorkerState::Ready
                || previous.is_some_and(|id| id.as_str() >= worker.worker_id.as_str())
            {
                return Err(Error::InvalidResponse);
            }
            previous = Some(&worker.worker_id);
        }
        Ok(workers)
    }

    /// Reuse the request identity after an uncertain exchange. An assignment
    /// receipt describes the original placement, which may since have expired.
    ///
    /// # Errors
    /// Refuses failed exchanges, foreign scope and a revision other than the next one.
    pub async fn assign(&self, request: &AssignScope) -> Result<Assignment, Error> {
        let assignment: Assignment = self
            .transport
            .post(endpoints::WORKFLOW_ASSIGN, request)
            .await?;
        let revision = request
            .expected_revision
            .map_or(0, Revision::get)
            .checked_add(1);
        if assignment.app_id != request.app_id
            || assignment.worker_id != request.worker_id
            || revision != Some(assignment.revision.get())
        {
            return Err(Error::InvalidResponse);
        }
        Ok(assignment)
    }

    /// # Errors
    /// Refuses failed exchanges and unordered, repeated or pre-cursor app identities.
    pub async fn recovery_scopes(&self, request: &ScopePage) -> Result<Vec<AppId>, Error> {
        let apps: Vec<AppId> = self
            .transport
            .post(endpoints::WORKFLOW_RECOVERY, request)
            .await?;
        let mut previous = request.after.as_ref();
        for app in &apps {
            if previous.is_some_and(|id| id.as_str() >= app.as_str()) {
                return Err(Error::InvalidResponse);
            }
            previous = Some(app);
        }
        Ok(apps)
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
