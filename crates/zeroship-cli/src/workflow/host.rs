//! The workflow host thread: the creator engine and the ordinary job consumer
//! over the app's own database and storage. Manager metadata stays on the
//! manager thread, reached through its client. Neither side opens the other's
//! storage, and no loop here scans the creator journal for runnable work.

#![expect(
    clippy::future_not_send,
    reason = "the workflow host owns its compio thread"
)]

use super::manager::{self, LocalPublisher, LocalTransport, ManagerClient, ManagerThread};
use crate::deployment::AppDeployment;
use futures::{
    future::{Either, LocalBoxFuture, Shared},
    FutureExt,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc, time::Duration};
use zeroship_bundle::LoadedWorker;
use zeroship_core::{app_id::AppId, workflow_coordination::AssignedScope, workflow_jobs::JobSpec};
use zeroship_runtime::{NativePlugin, RuntimeLimits};
use zeroship_workflow::{
    service::{
        runner::{
            consumer::{ConsumerBindings, ConsumerScope, JobConsumer},
            delivery::JobTransport,
            TaskExecutor,
        },
        schema,
        store::HostStorage,
        AppBackend, AppPolicy, AppWorkflows, HostPolicies, PolicySnapshot, WorkerIdentity,
        WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_v8::{AppRuntimeLoader, V8TaskExecutor};

const PUBLICATION_PAGE: u32 = 64;
const APPLIED_POLL: Duration = Duration::from_millis(20);

/// Wraps the delivery transport and executor built on the host thread.
/// The CLI uses both unchanged; tests observe them without another host.
pub trait Composition: Send + 'static {
    type Transport: JobTransport + 'static;

    fn transport(&self, transport: LocalTransport) -> Self::Transport;

    fn executor(&self, executor: Rc<dyn TaskExecutor>) -> Rc<dyn TaskExecutor> {
        executor
    }
}

#[derive(Debug)]
pub struct Production;

impl Composition for Production {
    type Transport = LocalTransport;

    fn transport(&self, transport: LocalTransport) -> LocalTransport {
        transport
    }
}

/// Everything the host needs, moved onto its thread.
pub struct Settings {
    pub app: AppId,
    pub config: super::LocalConfig,
    pub deployment: AppDeployment,
    pub storage: HostStorage,
    pub env_vars: HashMap<String, String>,
    pub peers: Vec<Arc<dyn NativePlugin>>,
    pub limits: RuntimeLimits,
}

/// A host whose placement, deployment selection and recovery responsibility
/// are established, ready to run its loops.
pub struct Opened<T: JobTransport> {
    pub api: AppWorkflows,
    pub backend: AppBackend,
    pub executable: Option<LoadedWorker>,
    /// The selected deployment's Activation job, whose creator receipt
    /// confirms that new work uses that deployment.
    pub activation: Option<JobSpec>,
    pub host: Host<T>,
}

pub struct Host<T: JobTransport> {
    api: AppWorkflows,
    manager: ManagerClient,
    thread: ManagerThread,
    consumer: JobConsumer<T>,
    executor: Rc<dyn TaskExecutor>,
    placement: RefCell<Placement>,
    wake: flume::Receiver<()>,
    renew_every: Duration,
}

struct Placement {
    scope: AssignedScope,
    binding: ConsumerScope,
}

type Stop<'a> = Shared<LocalBoxFuture<'a, ()>>;

/// Compose storage, manager and consumer, publish the retained deployment and
/// establish recovery responsibility. Nothing here waits for delivered work.
///
/// # Errors
/// Refuses incompatible storage, invalid bundles and unavailable metadata.
pub async fn open<C: Composition>(
    settings: Settings,
    composition: C,
) -> Result<Opened<C::Transport>, WorkflowServiceError> {
    let Settings {
        app,
        config,
        deployment,
        storage,
        env_vars,
        peers,
        limits,
    } = settings;
    let store = storage.open().await?;
    schema::initialize_local(&store).await?;
    let (manager, thread) =
        manager::spawn(deployment.platform().to_path_buf(), config.manager_options()).await?;
    let policies = Arc::new(HostPolicies::default());
    let policy = policies.bind(app.clone())?;
    policy
        .begin_refresh()?
        .install(PolicySnapshot::configuration(
            1.try_into().expect("initial host policy revision"),
            AppPolicy::default(),
        )?)?;
    let service = WorkflowService::open(Rc::new(store), policies)
        .await?
        .with_payload_storage(storage.objects)?
        .with_deployments(
            deployment
                .artifacts(config.max_source_bytes)?
                .with_hold_client(Rc::new(manager.journal_holds(&app))),
        );
    let api = service.register_app(&policy).await?;
    let scope = manager.place(&app).await?;
    let installed = deployment
        .load(&app, config.max_archive_bytes, config.max_source_bytes)
        .await?;
    let activation = if let Some(installed) = &installed {
        let id = manager
            .record(&app, installed.hash.clone(), installed.manifest.clone())
            .await?;
        let schedules = installed.executable.declarations().manager_schedules();
        Some(manager.publish(&app, &id, schedules).await?)
    } else {
        manager.selected(&app).await?
    };
    if let Some(activation) = &activation {
        manager.ensure_recovery(&app, activation).await?;
    }
    let (wake_sender, wake) = flume::bounded(1);
    let hint = wake_sender.clone();
    let backend = api
        .clone()
        .into_backend(config.payloads.max_payload_bytes)?
        .with_commit_hint(Arc::new(move || {
            let _ = hint.try_send(());
        }));
    let env = zeroship_runtime::serve::app_env_from_prefixed_vars(&env_vars);
    let loader = Rc::new(AppRuntimeLoader::new(
        backend.clone(),
        env_vars,
        env,
        peers,
        limits,
    )?);
    let tasks = Rc::new(api.tasks(WorkerIdentity::new(manager.worker().as_str().to_owned())?));
    let executor =
        composition.executor(Rc::new(V8TaskExecutor::new(loader, tasks, config.payloads)?));
    let consumer = JobConsumer::new(
        Rc::new(composition.transport(manager.transport(wake_sender.clone()))),
        manager.worker().clone(),
        config.consumer_options(),
    )?;
    let binding = ConsumerScope::new(api.clone(), scope.clone(), executor.clone())?;
    consumer.bindings().replace(vec![binding.clone()])?;
    // A previous process may have committed intents it never published.
    let _ = wake_sender.try_send(());
    Ok(Opened {
        api: api.clone(),
        backend,
        executable: installed.map(|installed| installed.executable.into_executable()),
        activation: activation.map(|activation| activation.job),
        host: Host {
            api,
            manager,
            thread,
            consumer,
            executor,
            placement: RefCell::new(Placement { scope, binding }),
            wake,
            renew_every: config.renew_interval(),
        },
    })
}

/// Wait until the creator has committed the delivered activation receipt.
/// Until then, new starts could still select an older deployment.
///
/// # Errors
/// Reports journal failures and an activation that did not finish in time.
pub async fn applied(
    api: &AppWorkflows,
    activation: &JobSpec,
    timeout: Duration,
) -> Result<(), WorkflowServiceError> {
    compio::time::timeout(timeout, async {
        loop {
            match api.job_receipt(activation).await {
                Ok(Some(_)) => return Ok(()),
                // Another connection's commit can refuse this read's writer
                // reservation for a moment; the receipt check simply repeats.
                Ok(None) | Err(WorkflowServiceError::Unavailable(_)) => {
                    compio::time::sleep(APPLIED_POLL).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(|_| {
        WorkflowServiceError::Unavailable(
            "the app deployment's workflow activation was not delivered".into(),
        )
    })?
}

impl<T: JobTransport> Host<T> {
    /// Consume delivered jobs, renew placement and publish committed intents
    /// until `stop`. Returns after execution joins and the manager stops.
    pub async fn run_until(self, stop: impl std::future::Future<Output = ()>) {
        let Self {
            api,
            manager,
            thread,
            mut consumer,
            executor,
            placement,
            wake,
            renew_every,
        } = self;
        let stop = stop.boxed_local().shared();
        let bindings = consumer.bindings();
        futures::join!(
            consumer.run_until(stop.clone()),
            place(
                &manager,
                &api,
                &executor,
                &bindings,
                &placement,
                renew_every,
                stop.clone()
            ),
            publish(&api, &manager, &placement, &wake, stop.clone()),
        );
        // Executions have joined; nothing new may be placed on this process.
        match compio::time::timeout(renew_every, manager.drain()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    code = error.code(),
                    "workflow worker could not report draining"
                );
            }
            Err(_) => tracing::warn!("workflow worker draining report timed out"),
        }
        // The manager finishes in-flight operations and its current pass.
        drop(thread);
    }
}

/// Keep this worker registered and the app placed on it. A refused placement
/// is replaced by the next revision; retired consumer bindings are rebuilt.
async fn place(
    manager: &ManagerClient,
    api: &AppWorkflows,
    executor: &Rc<dyn TaskExecutor>,
    bindings: &ConsumerBindings,
    placement: &RefCell<Placement>,
    every: Duration,
    stop: Stop<'_>,
) {
    loop {
        if stopped(stop.clone(), compio::time::sleep(every))
            .await
            .is_none()
        {
            return;
        }
        let (scope, retired) = {
            let current = placement.borrow();
            (current.scope.clone(), current.binding.is_retired())
        };
        let next = match stopped(stop.clone(), manager.renew(&scope)).await {
            None => return,
            Some(Ok(())) if !retired => continue,
            Some(Ok(())) => scope,
            Some(Err(
                WorkflowServiceError::PermissionDenied | WorkflowServiceError::Conflict(_),
            )) => match stopped(stop.clone(), manager.replace(&scope)).await {
                None => return,
                Some(Ok(next)) => next,
                Some(Err(error)) => {
                    tracing::warn!(code = error.code(), "workflow placement not replaced");
                    continue;
                }
            },
            Some(Err(error)) => {
                tracing::warn!(code = error.code(), "workflow placement not renewed");
                continue;
            }
        };
        let installed = ConsumerScope::new(api.clone(), next.clone(), executor.clone())
            .and_then(|binding| {
                bindings.replace(vec![binding.clone()])?;
                Ok(binding)
            });
        match installed {
            Ok(binding) => {
                *placement.borrow_mut() = Placement {
                    scope: next,
                    binding,
                };
            }
            Err(error) => {
                tracing::warn!(code = error.code(), "workflow consumer binding not replaced");
            }
        }
    }
}

/// Publish intents committed by ingress and delivered jobs as soon as they are
/// hinted. Failure leaves them pending for the manager's reconciliation job.
async fn publish(
    api: &AppWorkflows,
    manager: &ManagerClient,
    placement: &RefCell<Placement>,
    wake: &flume::Receiver<()>,
    stop: Stop<'_>,
) {
    while stopped(stop.clone(), wake.recv_async()).await == Some(Ok(())) {
        while wake.try_recv().is_ok() {}
        let publisher = manager.publisher(placement.borrow().scope.clone());
        match stopped(stop.clone(), publish_pending(api, &publisher)).await {
            None => return,
            Some(Ok(())) => {}
            Some(Err(error)) => {
                tracing::warn!(
                    code = error.code(),
                    "workflow publication left to manager reconciliation"
                );
            }
        }
    }
}

async fn publish_pending(
    api: &AppWorkflows,
    publisher: &LocalPublisher<'_>,
) -> Result<(), WorkflowServiceError> {
    let mut after = None;
    loop {
        let page = api.pending_jobs(after.as_ref(), PUBLICATION_PAGE).await?;
        for job in &page {
            api.publish_job(&job.id, publisher).await?;
        }
        if page.len() < PUBLICATION_PAGE as usize {
            return Ok(());
        }
        after = page.last().map(|job| job.id.clone());
    }
}

async fn stopped<T>(stop: Stop<'_>, work: impl std::future::Future<Output = T>) -> Option<T> {
    match futures::future::select(stop, work.boxed_local()).await {
        Either::Left(((), _)) => None,
        Either::Right((value, _)) => Some(value),
    }
}
