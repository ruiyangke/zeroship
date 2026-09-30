//! The deployment source a journal holder activates one app's deployment
//! through, with deterministic answers in place of Control's.
//!
//! It has the shape this service's own lane is built with: the retention
//! authority and the asserted manifest summary, and no artifact store. The
//! answers are Control's for a deployment that exists and is held. Retention
//! protocol behavior and the asserted summary Control derives are exercised by
//! the engine and Control suites.

use std::rc::Rc;
use zeroship_core::{
    app_id::AppId,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::DeploymentId,
};
use zeroship_workflow::{
    deploy_registrations::DeployRegistrationSource,
    deployment_holds::{AssignedHolds, DeploymentHoldClient},
    service::{AppDeployments, DeployRegistration},
    WorkflowServiceError,
};

/// A host serving `app` alone, whose Control holds `registration` as that app's
/// deployment.
pub fn asserted(app: &AppId, registration: &DeployRegistration) -> AppDeployments {
    let holds = Held {
        scope: HoldScope::for_app(app.clone()),
        hash: registration.hash.clone(),
    };
    AppDeployments::holds_only(Rc::new(AssignedHolds::new(Rc::new(holds))))
        .with_registrations(Rc::new(Asserted(app.clone(), registration.clone())))
}

/// Acknowledges every transition for the deployment it holds.
#[derive(Debug)]
struct Held {
    scope: HoldScope,
    hash: String,
}

impl Held {
    fn receipt(
        &self,
        deployment: &str,
        generation: HoldGeneration,
        state: HoldState,
    ) -> HoldReceipt {
        HoldReceipt {
            app_id: self.scope.app().clone(),
            deploy_id: deployment.to_owned(),
            holder_id: self.scope.holder().to_owned(),
            generation,
            state,
            deploy_hash: self.hash.clone(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for Held {
    fn scope(&self) -> &HoldScope {
        &self.scope
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        Ok(self.receipt(deployment, generation, HoldState::Held))
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        Ok(self.receipt(deployment, generation, HoldState::Released))
    }
}

/// Answers for its one app and deployment and refuses any other, as Control
/// refuses a deployment its catalog does not hold for the app asked about.
struct Asserted(AppId, DeployRegistration);

#[async_trait::async_trait(?Send)]
impl DeployRegistrationSource for Asserted {
    async fn registration(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
    ) -> Result<DeployRegistration, WorkflowServiceError> {
        if *app != self.0 || deployment.as_str() != self.1.id {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        Ok(self.1.clone())
    }
}
