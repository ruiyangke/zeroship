//! Deterministic hold responses for tests of metadata and host permissions.
//! Retention protocol behavior is exercised by the manager and Control suites.

use std::{future::Future, pin::Pin, rc::Rc};
use zeroship_core::{
    app_id::AppId,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::DeploymentId,
};
use zeroship_workflow_manager::{retention::HoldClient, Error};

pub fn client() -> Rc<dyn HoldClient> {
    Rc::new(Holds)
}

#[derive(Debug)]
struct Holds;
impl HoldClient for Holds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        Box::pin(async move { Ok(receipt(app, deployment, generation, HoldState::Held)) })
    }
    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        Box::pin(async move { Ok(receipt(app, deployment, generation, HoldState::Released)) })
    }
}

fn receipt(
    app: &AppId,
    deployment: &DeploymentId,
    generation: HoldGeneration,
    state: HoldState,
) -> HoldReceipt {
    HoldReceipt {
        app_id: app.clone(),
        deploy_id: deployment.as_str().to_owned(),
        holder_id: HoldScope::for_queue(app.clone()).holder().to_owned(),
        generation,
        state,
        deploy_hash: "a".repeat(64),
    }
}
