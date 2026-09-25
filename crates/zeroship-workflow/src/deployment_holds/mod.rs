//! Deployment retention clients for the hosts that hold a journal: a customer
//! worker on an app placement, and the workflow service that holds the journals
//! themselves. Platform catalog storage belongs to the workflow manager and is
//! supplied through the host's scoped client.

use crate::WorkflowServiceError;
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};
pub use zeroship_core::workflow_deployments::{
    HoldGeneration, HoldReceipt, HoldRequest, HoldScope, HoldState,
};
use zeroship_core::{app_id::AppId, service_peers::ServiceAuth};
use zeroship_workflow_client::Options;

mod remote;
pub use remote::RemoteDeploymentHolds;

/// Host-authenticated access to deployment metadata for a single app journal.
/// A customer worker receives this client, never the platform database.
#[async_trait::async_trait(?Send)]
pub trait DeploymentHoldClient {
    fn scope(&self) -> &HoldScope;
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError>;
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError>;
}

/// The deployment retention authority a host grants its workflow service. The
/// host states which apps it serves here and nowhere else, and the service
/// reaches a hold client only by asking it for one.
pub trait DeploymentHoldAuthority {
    /// The retention client for one app.
    ///
    /// # Errors
    /// Refuses an app this host does not serve.
    fn client(&self, app: &AppId) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError>;
}

/// The authority of a host that runs one assigned app.
///
/// A customer worker on an app placement holds this, and so does a local host
/// on the app it serves. The client's own scope names that app, so the scoping
/// has one statement and no roster to widen.
pub struct AssignedHolds(Rc<dyn DeploymentHoldClient>);

impl std::fmt::Debug for AssignedHolds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssignedHolds")
            .field("app", self.0.scope().app())
            .finish()
    }
}

impl AssignedHolds {
    /// Serve exactly the app `client` is scoped to.
    #[must_use]
    pub fn new(client: Rc<dyn DeploymentHoldClient>) -> Self {
        Self(client)
    }
}

impl DeploymentHoldAuthority for AssignedHolds {
    fn client(&self, app: &AppId) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError> {
        if self.0.scope().app() == app {
            Ok(Rc::clone(&self.0))
        } else {
            Err(WorkflowServiceError::PermissionDenied)
        }
    }
}

/// The authority of a host that holds the journals of many apps and no
/// placement on any of them.
///
/// WHAT BOUNDS THE APPS: every app whose journal this service holds, which is
/// every app in it. There is no roster and this type does not invent one. The
/// service's journal is one schema told apart by `app_id` columns, so a roster
/// here would be a second answer to a question the journal already answers, and
/// a stale one - an app registered a moment ago would be refused a hold by a set
/// this process had already built.
///
/// What is bounded is the AUTHORITY each client carries, which is the part that
/// matters: a client is scoped to the one app it was asked for, so the operation
/// holding it can only act on that app's deployments, and Control refuses a
/// deployment that does not belong to the app the scope names. Asking for a
/// second app therefore grants nothing the service's role did not already have.
/// Every caller inside the engine asks for the app whose journal rows it is
/// operating on and for no other.
pub struct ServiceHolds {
    control_url: String,
    auth: Arc<ServiceAuth>,
    options: Options,
    /// One client per app, because each carries a pooled HTTP connection to
    /// Control that outlives a single sweep.
    clients: RefCell<HashMap<AppId, Rc<dyn DeploymentHoldClient>>>,
}

impl std::fmt::Debug for ServiceHolds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceHolds")
            .field("apps", &self.clients.borrow().len())
            .finish_non_exhaustive()
    }
}

impl ServiceHolds {
    /// Mint journal-hold clients against `control_url` under the role `auth`
    /// signs for.
    ///
    /// The signer is checked when the first client is minted rather than here,
    /// by the constructor that checks it: this type holds no credential of its
    /// own and would only be restating that refusal in a second place.
    #[must_use]
    pub fn new(control_url: String, auth: Arc<ServiceAuth>, options: Options) -> Self {
        Self {
            control_url,
            auth,
            options,
            clients: RefCell::new(HashMap::new()),
        }
    }
}

impl DeploymentHoldAuthority for ServiceHolds {
    fn client(&self, app: &AppId) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError> {
        if let Some(client) = self.clients.borrow().get(app) {
            return Ok(Rc::clone(client));
        }
        let client: Rc<dyn DeploymentHoldClient> = Rc::new(RemoteDeploymentHolds::asserted(
            &self.control_url,
            self.auth.clone(),
            app.clone(),
            self.options.clone(),
        )?);
        self.clients
            .borrow_mut()
            .insert(app.clone(), Rc::clone(&client));
        Ok(client)
    }
}
