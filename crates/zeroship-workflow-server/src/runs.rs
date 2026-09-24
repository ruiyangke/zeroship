//! The journal this service answers creator-facing run calls from.
//!
//! The engine is opened over the service's own `workflow_manager` schema rather
//! than over a creator database, so every app whose workflows live here is
//! served by one store and told apart by the `app_id` columns inside it.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

use futures::{future::LocalBoxFuture, lock::Mutex};
use zeroship_core::{app_id::AppId, schema_name::SchemaName, workflow_coordination::Revision};
use zeroship_data_orm::{
    binding::DbBinding, connection::ConnectionFactory, encryption::ProjectKeySource,
};
use zeroship_workflow::{
    service::{
        store::OrmStore, AppWorkflows, HostPolicies, IngressEpochs, PolicyBinding, PolicySnapshot,
        WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_manager::{
    policy::{PolicyObservation, PolicySource},
    recovery::Recovery,
    Error as ManagerError,
};

/// The schema the journal is installed into, and the one this service holds DML
/// on. `Coordinator::verify` separately refuses to start when the same login
/// also holds CREATE here.
const JOURNAL_SCHEMA: &str = "workflow_manager";

/// The journal, the policy registry its bindings come from, and the recovery
/// scope its ingress epochs are established against.
///
/// One per HTTP worker thread. The store is `!Send` -- it holds an `Rc<dyn
/// Backend>` -- so the service cannot be shared, and giving each thread its own
/// registry alongside it means the one call that retires a generation,
/// `HostPolicies::bind`, has a single writer per registry without any lock of
/// this module's own. The ingress cache is per thread for the same reason, and
/// needs no lock of its own either.
#[derive(Debug)]
pub struct RunService {
    journal: WorkflowService,
    policies: Arc<HostPolicies>,
    recovery: Recovery,
    ingress: RefCell<HashMap<AppId, Rc<ServiceIngress>>>,
}

impl RunService {
    /// Open the journal over the service's own schema.
    ///
    /// `recovery` owns the same app scopes this journal serves, so acceptance
    /// establishes its ingress epoch directly rather than through a manager
    /// client: here the manager and the journal are the same process.
    ///
    /// # Errors
    /// Reports an unusable schema name, an unusable url, and journal storage
    /// that refuses to verify.
    pub async fn connect(url: &str, recovery: Recovery) -> Result<Self, WorkflowServiceError> {
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
        Ok(Self {
            journal,
            policies,
            recovery,
            ingress: RefCell::new(HashMap::new()),
        })
    }

    /// Bind `app` to this journal under the admission policy just observed.
    ///
    /// The observation is taken first, so everything touching the registry is
    /// synchronous: nothing can interleave between reserving a refresh and
    /// installing it, which is the one ordering `PolicyRefresh` refuses,
    /// nothing can interleave between finding an app unbound and binding it,
    /// which is the one call that retires a generation another request may be
    /// operating under, and nothing can interleave between reading the held
    /// ingress epoch and reinstalling it, which is what makes carrying it
    /// forward a read-modify-write no concurrent request can tear.
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
        let observed = source.observe(app).await.map_err(|_| {
            WorkflowServiceError::Unavailable("workflow policy is unavailable".into())
        })?;
        let (binding, rebound) = match self.policies.current_binding(app) {
            Ok(binding) => (binding, false),
            Err(_) => (self.policies.bind(app.clone())?, true),
        };
        // CARRY THE INGRESS EPOCH FORWARD. This observation is not an authority
        // on it: the policy source answers what the control plane granted, and
        // the epoch names the manager's recovery responsibility, which that
        // source has never seen. `PolicySnapshot::lease` therefore starts every
        // snapshot at `None`, and installing that verbatim would erase, on the
        // very next request, the epoch an establishment had just obtained -
        // leaving `signal` and `transition` permanently fenced no matter how
        // often they established.
        //
        // ABSENCE OF INFORMATION IS NOT INFORMATION ABOUT ABSENCE, and the
        // distinction is why this belongs here and not inside `install`.
        // `AssignedPolicies` installs a manager lease, where a `None` epoch is
        // the manager AUTHORITATIVELY saying no responsibility is open;
        // preserving a stale epoch there would defeat the fence outright. Two
        // install paths that look alike must not be unified: one is silent
        // about the epoch, the other is decisive about it.
        //
        // What licenses carrying it forward at all is that the epoch is a claim
        // rechecked at use time, never a cached grant. `require_open_epoch`
        // reads the journal's `closed_epoch` inside the caller's transaction on
        // every mutation and admits only while `held > closed`, so raising the
        // closed epoch fences a carried-forward value by itself, with no policy
        // reinstall involved.
        install(&binding, &observed, binding.ingress_epoch())?;
        let ingress = self.ingress(app, &binding, &observed, rebound);
        Ok(self.journal.bind_app(&binding)?.with_ingress(ingress))
    }

    /// The ingress this app establishes epochs through, refreshed to the
    /// observation just installed.
    ///
    /// One per app rather than one per call, so `IngressEpochs::establish`
    /// serializes its exchange against every other request's rather than only
    /// against its own. A rebinding retires the generation the cached ingress
    /// holds, so its entry is replaced rather than reused.
    fn ingress(
        &self,
        app: &AppId,
        binding: &PolicyBinding,
        observed: &PolicyObservation,
        rebound: bool,
    ) -> Rc<ServiceIngress> {
        let mut cache = self.ingress.borrow_mut();
        if rebound {
            cache.remove(app);
        }
        let ingress = Rc::clone(cache.entry(app.clone()).or_insert_with(|| {
            Rc::new(ServiceIngress {
                recovery: self.recovery.clone(),
                app: app.clone(),
                binding: binding.clone(),
                observed: RefCell::new(observed.clone()),
                reporting: Rc::new(Cell::new(false)),
                exchanges: Mutex::new(()),
            })
        }));
        *ingress.observed.borrow_mut() = observed.clone();
        ingress
    }
}

/// Install `observed` into `binding`, attaching the ingress epoch the caller
/// determined. Every install this service performs goes through here, so the
/// epoch is always a deliberate argument rather than a defaulted field.
fn install(
    binding: &PolicyBinding,
    observed: &PolicyObservation,
    epoch: Option<Revision>,
) -> Result<(), WorkflowServiceError> {
    binding.begin_refresh()?.install(
        PolicySnapshot::lease(
            observed.revision(),
            observed.policy().clone(),
            observed.expires_at(),
        )?
        .with_ingress_epoch(epoch),
    )
}

/// Establishes ingress epochs against this service's own recovery scope.
///
/// The local development host reaches the same `Recovery::establish` through
/// its manager client; this host owns the scope directly, because the manager
/// and the journal are the same process here.
#[derive(Debug)]
struct ServiceIngress {
    recovery: Recovery,
    app: AppId,
    binding: PolicyBinding,
    /// The observation `RunService::app` last installed into `binding`.
    ///
    /// Establishment reinstalls whatever is here when it reaches its install,
    /// never one captured earlier. A stale deadline reinstalled over a newer
    /// one reads as a shortened lease, which retires the binding's epoch and
    /// cancels every operation bound to it.
    observed: RefCell<PolicyObservation>,
    /// Set while a report of accepted ingress is in flight, so a burst of
    /// acceptances costs the manager one round trip rather than one each.
    reporting: Rc<Cell<bool>>,
    /// One exchange at a time, so no establishment supersedes another's
    /// refresh ticket while it waits on the manager.
    exchanges: Mutex<()>,
}

impl ServiceIngress {
    /// Obtain and install an epoch above `after`, or any open epoch when it
    /// names none. A newer epoch another acceptance already installed
    /// satisfies the call without asking the manager.
    ///
    /// # Errors
    /// Reports refused establishment, unavailable manager storage and a
    /// retired policy binding.
    #[expect(
        clippy::future_not_send,
        reason = "establishment runs on the thread that owns the binding and the queue"
    )]
    async fn establish_epoch(&self, after: Option<Revision>) -> Result<(), WorkflowServiceError> {
        let _exchange = self.exchanges.lock().await;
        if self
            .binding
            .ingress_epoch()
            .is_some_and(|held| after.is_none_or(|after| held > after))
        {
            return Ok(());
        }
        let admission = self.observed.borrow().policy().admission;
        let epoch = self
            .recovery
            .establish(&self.app, after, admission)
            .await
            .map_err(establishment_error)?;
        // RESERVE THE TICKET LAST, after the exchange rather than before it.
        // The local development host reserves first, so that a policy refresh
        // landing mid-exchange cannot overwrite the epoch it is about to
        // install; there, nothing else refreshes. Here `RunService::app`
        // refreshes on EVERY request, so reserving first would hand the race to
        // it and this establishment would lose its ticket and refuse.
        //
        // Reserving last is safe because monotonicity, not the ticket, is what
        // guards the install: a revision that moved under this exchange fails
        // the retained high water and is refused as a conflict, and an
        // unchanged one installs cleanly. The epoch then wins whichever side
        // lands first, because an `app` that runs afterwards carries it
        // forward.
        //
        // READ THE OBSERVATION HERE TOO, after the exchange rather than before
        // it, for a reason of the same shape. A request that reinstalled while
        // this one waited may have installed a later deadline -
        // `ControlPolicies::observe` returns a freshly read observation once
        // the cached one is due - and reinstalling the earlier deadline over it
        // reads as a SHORTENED lease, which retires the binding's epoch and
        // cancels every operation bound to it. Reading at the install leaves no
        // window, because the two are synchronous.
        install(&self.binding, &self.observed.borrow(), Some(epoch))
    }
}

impl IngressEpochs for ServiceIngress {
    fn establish(
        &self,
        after: Option<Revision>,
    ) -> LocalBoxFuture<'_, Result<(), WorkflowServiceError>> {
        Box::pin(self.establish_epoch(after))
    }

    fn accepted(&self) {
        // An app serving ingress is not idle, and an idle scope may be closed.
        // The report is a round trip and this is not an async call, so it is
        // detached; a failed one is not retried here, because the next
        // acceptance reports again and a scope closed in between reopens at a
        // newer epoch that establishment obtains.
        if self.reporting.replace(true) {
            return;
        }
        let recovery = self.recovery.clone();
        let app = self.app.clone();
        let reporting = Rc::clone(&self.reporting);
        compio::runtime::spawn(async move {
            let reported = recovery.note_ingress(&app).await;
            reporting.set(false);
            if let Err(error) = reported {
                tracing::warn!(
                    code = establishment_error(error).code(),
                    "workflow ingress activity not reported"
                );
            }
        })
        .detach();
    }
}

/// Carry a manager refusal into the engine's contract WITHOUT flattening a
/// refusal into an outage: the first is durable and wants an operator, the
/// second is transient and wants a retry. The match is wildcard-free, so a new
/// manager refusal stops compiling here rather than folding into a neighbour.
fn establishment_error(error: ManagerError) -> WorkflowServiceError {
    match error {
        ManagerError::Invalid => WorkflowServiceError::InvalidRequest(
            "workflow ingress establishment was refused as invalid".into(),
        ),
        ManagerError::Denied => WorkflowServiceError::PermissionDenied,
        ManagerError::Conflict => WorkflowServiceError::Conflict(
            "workflow ingress epoch conflicts with the manager".into(),
        ),
        ManagerError::Capacity => WorkflowServiceError::ResourceExhausted(
            "workflow ingress establishment was refused for capacity".into(),
        ),
        ManagerError::Timeout => WorkflowServiceError::Timeout,
        ManagerError::Unavailable => WorkflowServiceError::Unavailable(
            "workflow ingress establishment is unavailable".into(),
        ),
        // A storage contract failure is this host's own defect, not a condition
        // the caller can act on or retry into.
        ManagerError::Storage => WorkflowServiceError::Internal(
            "workflow ingress establishment storage contract failed".into(),
        ),
    }
}
