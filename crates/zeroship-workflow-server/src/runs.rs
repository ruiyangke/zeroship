//! The journal this service answers creator-facing run calls from.
//!
//! The engine is opened over the service's own `workflow_manager` schema rather
//! than over a creator database, so every app whose workflows live here is
//! served by one store and told apart by the `app_id` columns inside it.

use std::{rc::Rc, sync::Arc};

use zeroship_core::{app_id::AppId, schema_name::SchemaName};
use zeroship_data_orm::{
    binding::DbBinding, connection::ConnectionFactory, encryption::ProjectKeySource,
};
use zeroship_workflow::{
    service::{store::OrmStore, AppWorkflows, HostPolicies, PolicySnapshot, WorkflowService},
    WorkflowServiceError,
};
use zeroship_workflow_manager::policy::PolicySource;

/// The schema the journal is installed into, and the one this service holds DML
/// on. `Coordinator::verify` separately refuses to start when the same login
/// also holds CREATE here.
const JOURNAL_SCHEMA: &str = "workflow_manager";

/// The journal and the policy registry its bindings come from.
///
/// One per HTTP worker thread. The store is `!Send` -- it holds an `Rc<dyn
/// Backend>` -- so the service cannot be shared, and giving each thread its own
/// registry alongside it means the one call that retires a generation,
/// `HostPolicies::bind`, has a single writer per registry without any lock of
/// this module's own.
#[derive(Debug)]
pub struct RunService {
    journal: WorkflowService,
    policies: Arc<HostPolicies>,
}

impl RunService {
    /// Open the journal over the service's own schema.
    ///
    /// # Errors
    /// Reports an unusable schema name, an unusable url, and journal storage
    /// that refuses to verify.
    pub async fn connect(url: &str) -> Result<Self, WorkflowServiceError> {
        let schema = SchemaName::new(JOURNAL_SCHEMA).map_err(|_| {
            WorkflowServiceError::Internal("workflow journal schema name is invalid".into())
        })?;
        let factory = ConnectionFactory::for_platform_url(url).map_err(|_| {
            WorkflowServiceError::Unavailable("workflow journal url is unusable".into())
        })?;
        let store = OrmStore::connect(
            DbBinding::platform(JOURNAL_SCHEMA, "workflow-journal", schema),
            &factory,
            // The journal declares no encrypted column, so there is no project
            // key to resolve and none to supply.
            ProjectKeySource::unavailable(),
        )
        .await?;
        let policies = Arc::new(HostPolicies::default());
        let journal = WorkflowService::open(Rc::new(store), policies.clone()).await?;
        Ok(Self { journal, policies })
    }

    /// Bind `app` to this journal under the admission policy just observed.
    ///
    /// The observation is taken first, so everything touching the registry is
    /// synchronous: nothing can interleave between reserving a refresh and
    /// installing it, which is the one ordering `PolicyRefresh` refuses, and
    /// nothing can interleave between finding an app unbound and binding it,
    /// which is the one call that retires a generation another request may be
    /// operating under.
    ///
    /// The snapshot is a LEASE rather than configuration because the
    /// observation is genuinely time-bounded: `PolicyObservation` carries the
    /// instant its validity ends, and a binding's validity mode cannot change
    /// after its first install. Installing configuration here would discard
    /// that deadline permanently and leave the service admitting work against a
    /// policy it had stopped rechecking.
    ///
    /// Installing on every call is deliberate. There is no way to read an
    /// installed snapshot's revision back out, and none is needed: `install`
    /// compares against its own retained high water, and an unchanged revision
    /// with an unmoved deadline invalidates nothing, so a repeat is inert
    /// rather than disruptive to requests already in flight.
    ///
    /// # Errors
    /// Reports an unavailable policy source, a policy the platform refuses, a
    /// superseded or retired binding, and a binding this journal will not
    /// accept.
    pub async fn app(
        &self,
        source: &dyn PolicySource,
        app: &AppId,
    ) -> Result<AppWorkflows, WorkflowServiceError> {
        let observed = source
            .observe(app)
            .await
            .map_err(|_| WorkflowServiceError::Unavailable("workflow policy is unavailable".into()))?;
        let binding = match self.policies.current_binding(app) {
            Ok(binding) => binding,
            Err(_) => self.policies.bind(app.clone())?,
        };
        binding.begin_refresh()?.install(PolicySnapshot::lease(
            observed.revision(),
            observed.policy().clone(),
            observed.expires_at(),
        )?)?;
        self.journal.bind_app(&binding)
    }
}
