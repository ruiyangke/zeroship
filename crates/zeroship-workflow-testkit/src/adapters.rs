//! The workflow-typed adapters over this crate's plain catalog, journal server
//! and service bindings, expanded in the crate that uses them.
//!
//! The adapters name `zeroship_workflow` types - the registration, the host
//! bindings, the hold and registration traits, the journal store - so they
//! cannot be compiled here: this crate never depends on `zeroship-workflow`,
//! whose own unit tests use it. [`crate::workflow_fixtures!`] holds them once and
//! expands them where they are invoked, against whichever `zeroship_workflow`
//! that crate sees: `zeroship-workflow` expands them for its own unit tests,
//! where `zeroship_workflow` names the crate under test, and the runner, the
//! worker and the V8 binding each expand them in a test module of their own.
//! No shipped crate carries a test feature for them.

/// The crates the expanded adapters name, so an expansion site needs no
/// dependency of its own on them.
#[doc(hidden)]
pub mod __private {
    pub use async_trait;
    pub use compio_postgres;
    pub use serde_json;
    pub use tempfile;
    pub use zeroship_bundle;
    pub use zeroship_core;
    pub use zeroship_data_orm;
    pub use zeroship_workflow_manager;
}

/// Expand the workflow-typed fixture adapters - `deployment`, `journal`,
/// `service_binding` and the `manager_queue` re-export - as modules of the
/// invoking crate.
///
/// The invoking crate must see `zeroship_workflow` and
/// `zeroship_workflow_testkit` by those names.
#[macro_export]
macro_rules! workflow_fixtures {
    () => {
        pub use $crate::manager_queue;

        /// The workflow-typed adapter over the testkit's plain deployment
        /// catalog: the workflow service types - the registration, the host
        /// bindings, the hold and registration traits - over the plain catalog,
        /// converted at the edge.
        pub mod deployment {
            use std::{collections::BTreeMap, rc::Rc, sync::Arc};
            use $crate::adapters::__private::zeroship_bundle::BlobStore;
            use $crate::adapters::__private::zeroship_core::{
                app_id::AppId,
                workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope},
            };
            use $crate::deployments::{
                Catalog, Declaration, HoldError, HoldHandle, PublishError, RegistrationSource,
            };

            use zeroship_workflow::{
                deploy_registrations::DeployRegistrationSource,
                deployment_holds::{DeploymentHoldAuthority, DeploymentHoldClient},
                service::{AppDeployments, BundleDeclarations, DeployRegistration, WorkflowService},
                WorkflowServiceError,
            };

            pub use $crate::deployments::Sources;

            /// A catalog of real app artifacts with workflow-typed host bindings
            /// over it.
            pub struct Deployments {
                /// The artifact store this catalog's deployments are published to.
                pub source: Arc<dyn BlobStore>,
                /// The local platform file holding this catalog, for queues that
                /// hold deployments through it.
                pub platform: $crate::adapters::__private::zeroship_workflow_manager::local::LocalPlatform,
                catalog: Catalog,
            }

            impl Deployments {
                pub async fn new() -> Self {
                    let catalog = Catalog::new().await;
                    Self {
                        source: catalog.source.clone(),
                        platform: catalog.platform.clone(),
                        catalog,
                    }
                }

                /// A catalog over a caller-owned directory and artifact store.
                pub async fn with_source(
                    directory: Arc<$crate::adapters::__private::tempfile::TempDir>,
                    source: Arc<dyn BlobStore>,
                ) -> Self {
                    let catalog = Catalog::with_source(directory, source.clone()).await;
                    Self {
                        source,
                        platform: catalog.platform.clone(),
                        catalog,
                    }
                }

                pub fn client(&self, app: &AppId) -> OwnedClient {
                    self.client_for_scope(HoldScope::for_app(app.clone()))
                }

                pub fn client_for_scope(&self, scope: HoldScope) -> OwnedClient {
                    OwnedClient {
                        inner: self.catalog.client_for_scope(scope),
                    }
                }

                /// A host serving exactly `apps`, each through this catalog's own
                /// client.
                pub fn binding(&self, apps: &[&AppId]) -> AppDeployments {
                    self.hosting(apps, None)
                }

                /// The same host, with `client` standing in for the app it is
                /// scoped to.
                pub fn binding_with(
                    &self,
                    apps: &[&AppId],
                    client: Rc<dyn DeploymentHoldClient>,
                ) -> AppDeployments {
                    self.hosting(apps, Some(client))
                }

                fn hosting(
                    &self,
                    apps: &[&AppId],
                    client: Option<Rc<dyn DeploymentHoldClient>>,
                ) -> AppDeployments {
                    let hosted = HostedApps::new(
                        apps.iter()
                            .map(|app| Rc::new(self.client(app)) as Rc<dyn DeploymentHoldClient>)
                            .chain(client),
                    );
                    AppDeployments::new(self.source.clone(), 1024 * 1024, Rc::new(hosted)).unwrap()
                }

                /// A host holding the retention authority for `apps` and nothing
                /// else: no artifact store and no asserted registration source.
                ///
                /// The control arm for [`Self::asserted_binding`]. Every sweep
                /// that needs a manifest summary must refuse here, or a green on
                /// the asserted arm would only be saying the operation asks for
                /// nothing.
                pub fn holds_binding(&self, apps: &[&AppId]) -> AppDeployments {
                    AppDeployments::holds_only(Rc::new(HostedApps::new(
                        apps.iter()
                            .map(|app| Rc::new(self.client(app)) as Rc<dyn DeploymentHoldClient>),
                    )))
                }

                /// The same host, plus the asserted manifest summary. Still no
                /// artifact store: `AppDeployments::read` refuses on this binding
                /// by name.
                pub fn asserted_binding(&self, apps: &[&AppId]) -> AppDeployments {
                    self.holds_binding(apps)
                        .with_registrations(Rc::new(self.registrations()))
                }

                /// A registration source over this catalog's own `app_deploys`
                /// rows.
                ///
                /// It derives the summary the way Control's endpoint does, from
                /// the stored manifest and the hash beside it, so an engine test
                /// can exercise the asserted arm without an HTTP Control. That the
                /// DEPLOYED derivation agrees is a different claim, and
                /// `zeroship-control` asserts it against the real endpoint.
                #[must_use]
                pub fn registrations(&self) -> CatalogRegistrations {
                    CatalogRegistrations {
                        source: self.catalog.registration_source(),
                    }
                }

                /// Store `declaration`'s sources as a deployable artifact and
                /// record its catalog row, returning the registration with its
                /// pinned hash.
                ///
                /// # Errors
                /// When the stored row already names another app or another hash.
                pub async fn publish(
                    &self,
                    app: &AppId,
                    declaration: &DeployRegistration,
                    sources: &Sources,
                ) -> Result<DeployRegistration, WorkflowServiceError> {
                    let plain = Declaration {
                        id: declaration.id.clone(),
                        hash: declaration.hash.clone(),
                        workflows: declaration.workflows.clone(),
                        schedules: declaration
                            .schedules
                            .iter()
                            .map(|schedule| $crate::adapters::__private::serde_json::to_value(schedule).unwrap())
                            .collect(),
                    };
                    let stored = self
                        .catalog
                        .publish(app, &plain, sources)
                        .await
                        .map_err(|error| match error {
                            PublishError::Conflict(message) => WorkflowServiceError::Conflict(message),
                        })?;
                    Ok(DeployRegistration {
                        id: stored.id,
                        hash: stored.hash,
                        workflows: stored.workflows,
                        schedules: stored
                            .schedules
                            .into_iter()
                            .map(|schedule| $crate::adapters::__private::serde_json::from_value(schedule).unwrap())
                            .collect(),
                    })
                }

                /// Publish a default deployment and activate it on `service`.
                ///
                /// # Errors
                /// When the publication or the activation is refused.
                pub async fn activate(
                    &self,
                    service: &WorkflowService,
                    app: &AppId,
                    declaration: &DeployRegistration,
                ) -> Result<(), WorkflowServiceError> {
                    let deployment = self.publish(app, declaration, &Sources::default()).await?;
                    service.activate_deploy(app, &deployment).await
                }

                /// Publish a default deployment for `app` and return its
                /// registration.
                pub async fn deploy(&self, app: &AppId) -> DeployRegistration {
                    self.publish(
                        app,
                        &DeployRegistration {
                            id: $crate::adapters::__private::zeroship_core::typed_id::generate("dep"),
                            hash: String::new(),
                            workflows: ["Example".into()].into(),
                            schedules: vec![],
                        },
                        &Sources::default(),
                    )
                    .await
                    .unwrap()
                }

                pub async fn assert_held(&self, app: &AppId, deployment: &str) {
                    self.catalog.assert_held(app, deployment).await;
                }

                /// Every holder gave this deployment back, so the platform
                /// collector's fence commits. The probe rolls back, leaving the
                /// catalog collectable.
                pub async fn assert_reclaimable(&self, app: &AppId, deployment: &str) {
                    self.catalog.assert_reclaimable(app, deployment).await;
                }
            }

            /// A test host serving a named set of apps. The set is stated once,
            /// where the host is built, so no later call can add an app to it.
            #[derive(Default)]
            pub struct HostedApps(BTreeMap<AppId, Rc<dyn DeploymentHoldClient>>);

            impl HostedApps {
                pub fn new(clients: impl IntoIterator<Item = Rc<dyn DeploymentHoldClient>>) -> Self {
                    Self(
                        clients
                            .into_iter()
                            .map(|client| (client.scope().app().clone(), client))
                            .collect(),
                    )
                }
            }

            impl DeploymentHoldAuthority for HostedApps {
                fn client(
                    &self,
                    app: &AppId,
                ) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError> {
                    self.0
                        .get(app)
                        .cloned()
                        .ok_or(WorkflowServiceError::PermissionDenied)
                }
            }

            /// Control's half of the asserted registration, over this fixture's
            /// catalog.
            ///
            /// It reads the stored manifest and the hash the row was accepted
            /// under, re-verifies the binding between them, and parses the
            /// declarations - the same three steps
            /// `DeploymentHoldApi::registration` takes. It reaches no blob store:
            /// the manifest it parses is the catalog column, not an artifact.
            #[derive(Clone)]
            pub struct CatalogRegistrations {
                source: RegistrationSource,
            }

            #[$crate::adapters::__private::async_trait::async_trait(?Send)]
            impl DeployRegistrationSource for CatalogRegistrations {
                async fn registration(
                    &self,
                    app: &AppId,
                    deployment: &$crate::adapters::__private::zeroship_core::workflow_jobs::DeploymentId,
                ) -> Result<DeployRegistration, WorkflowServiceError> {
                    let (hash, manifest) = self
                        .source
                        .registration_row(app, deployment.as_str())
                        .await
                        .ok_or(WorkflowServiceError::PermissionDenied)?;
                    let manifest =
                        $crate::adapters::__private::zeroship_bundle::verify_deployment_manifest(&manifest, &hash)
                            .map_err(|_| WorkflowServiceError::Internal("catalog manifest".into()))?;
                    Ok(BundleDeclarations::parse(&manifest)
                        .map_err(|_| WorkflowServiceError::Internal("catalog declarations".into()))?
                        .registration(deployment.as_str().to_owned(), hash))
                }
            }

            /// A source that answers about the right deployment under the WRONG
            /// hash.
            ///
            /// The engine pins the hash from the hold it already took, so this is
            /// the arm that proves it compares rather than records what it was
            /// handed.
            pub struct RehashedRegistrations<T>(pub T, pub String);

            #[$crate::adapters::__private::async_trait::async_trait(?Send)]
            impl<T: DeployRegistrationSource> DeployRegistrationSource for RehashedRegistrations<T> {
                async fn registration(
                    &self,
                    app: &AppId,
                    deployment: &$crate::adapters::__private::zeroship_core::workflow_jobs::DeploymentId,
                ) -> Result<DeployRegistration, WorkflowServiceError> {
                    let mut registration = self.0.registration(app, deployment).await?;
                    registration.hash.clone_from(&self.1);
                    Ok(registration)
                }
            }

            #[derive(Clone)]
            pub struct OwnedClient {
                inner: HoldHandle,
            }

            #[$crate::adapters::__private::async_trait::async_trait(?Send)]
            impl DeploymentHoldClient for OwnedClient {
                fn scope(&self) -> &HoldScope {
                    self.inner.scope()
                }

                async fn acquire(
                    &self,
                    deployment: &str,
                    generation: HoldGeneration,
                ) -> Result<HoldReceipt, WorkflowServiceError> {
                    self.inner
                        .acquire(deployment, generation)
                        .await
                        .map_err(hold_error)
                }

                async fn release(
                    &self,
                    deployment: &str,
                    generation: HoldGeneration,
                ) -> Result<HoldReceipt, WorkflowServiceError> {
                    self.inner
                        .release(deployment, generation)
                        .await
                        .map_err(hold_error)
                }
            }

            fn hold_error(error: HoldError) -> WorkflowServiceError {
                match error {
                    HoldError::InvalidRequest(message) => WorkflowServiceError::InvalidRequest(message),
                    HoldError::Unauthenticated => WorkflowServiceError::Unauthenticated,
                    HoldError::PermissionDenied => WorkflowServiceError::PermissionDenied,
                    HoldError::Conflict(message) => WorkflowServiceError::Conflict(message),
                    HoldError::ResourceExhausted(message) => {
                        WorkflowServiceError::ResourceExhausted(message)
                    }
                    HoldError::Unavailable(message) => WorkflowServiceError::Unavailable(message),
                    HoldError::Timeout => WorkflowServiceError::Timeout,
                    HoldError::Internal(message) => WorkflowServiceError::Internal(message),
                }
            }
        }

        /// Binding an app's policy generation the way a trusted host would.
        ///
        /// Both halves of the workflow engine open apps this way in their tests,
        /// so the extension trait is shared: `WorkflowService` is foreign to one
        /// of them and an inherent `impl` on it does not compile there.
        pub mod service_binding {
            use std::{future::Future, sync::Arc};
            use $crate::adapters::__private::zeroship_core::app_id::AppId;

            use zeroship_workflow::{
                service::{AppWorkflows, HostPolicies, PolicyBinding, PolicySnapshot, WorkflowService},
                WorkflowServiceError,
            };

            pub trait ServiceFixture {
                /// The app's live binding, created when the host has not bound it
                /// yet.
                fn fixture_binding(&self, app: &AppId) -> Result<PolicyBinding, WorkflowServiceError>;

                /// Open the app on its live binding.
                fn fixture_app(&self, app: AppId) -> AppWorkflows;

                /// Install policy and register the app, as a host does on first
                /// contact.
                fn fixture_register(
                    &self,
                    app: &AppId,
                    snapshot: PolicySnapshot,
                ) -> impl Future<Output = Result<(), WorkflowServiceError>>;

                /// Install over the existing binding without re-registering the
                /// app, so a fixture can reissue policy after its setup already
                /// ran.
                fn fixture_install(
                    &self,
                    app: &AppId,
                    snapshot: PolicySnapshot,
                ) -> Result<(), WorkflowServiceError>;
            }

            impl ServiceFixture for WorkflowService {
                fn fixture_binding(&self, app: &AppId) -> Result<PolicyBinding, WorkflowServiceError> {
                    binding(self.policies(), app)
                }

                fn fixture_app(&self, app: AppId) -> AppWorkflows {
                    let binding = binding(self.policies(), &app).unwrap();
                    self.bind_app(&binding).unwrap()
                }

                async fn fixture_register(
                    &self,
                    app: &AppId,
                    snapshot: PolicySnapshot,
                ) -> Result<(), WorkflowServiceError> {
                    let binding = binding(self.policies(), app)?;
                    binding.begin_refresh()?.install(snapshot)?;
                    self.register_app(&binding).await.map(|_| ())
                }

                fn fixture_install(
                    &self,
                    app: &AppId,
                    snapshot: PolicySnapshot,
                ) -> Result<(), WorkflowServiceError> {
                    binding(self.policies(), app)?
                        .begin_refresh()?
                        .install(snapshot)
                }
            }

            fn binding(
                policies: &Arc<HostPolicies>,
                app: &AppId,
            ) -> Result<PolicyBinding, WorkflowServiceError> {
                policies
                    .current_binding(app)
                    .or_else(|_| policies.bind(app.clone()))
            }
        }

        /// The workflow journal on `SQLite` and `PostgreSQL`, opened where every
        /// host opens it, and apps registered on it.
        ///
        /// Every store here comes from `HostStorage`, so it is on
        /// `journal_binding()` exactly as the production service's and
        /// `zeroship serve`'s are. On `PostgreSQL` the journal carries the
        /// posture `db/migrations-ts/20260919000000_workflow_journal.ts` leaves
        /// it in: `JOURNAL_SCHEMA` and its tables owned by the migration role,
        /// and the service login holding schema usage and table DML and nothing
        /// else. The plain server, roles and grants live in the testkit's
        /// `journal_server`; this module supplies the schema's DDL and the
        /// `OrmStore` the workflow service reads.
        pub mod journal {
            use std::{
                path::{Path, PathBuf},
                rc::Rc,
                sync::Arc,
            };

            use $crate::adapters::__private::zeroship_core::{app_id::AppId, typed_id};
            use $crate::adapters::__private::zeroship_data_orm::connection::ConnectionFactory;
            use $crate::journal_server::{self, JournalServer};
            use $crate::manager_queue::open_epoch;

            use super::{deployment::Deployments, service_binding::ServiceFixture};
            use zeroship_workflow::service::{
                schema,
                store::{HostStorage, OrmStore, SchemaName, JOURNAL_SCHEMA},
                AppPolicy, DeployRegistration, HostPolicies, PolicySnapshot, WorkflowService,
            };

            pub use $crate::journal_server::{connect, JOURNAL_LOGIN, JOURNAL_OWNER};

            /// The file the journal binding keeps the journal in, inside
            /// `directory`.
            ///
            /// On `SQLite` the binding's schema is the `ATTACH` alias, and the
            /// backend keeps an attached alias in `zs-<alias>.sqlite` beside the
            /// session file.
            pub fn journal_file(directory: &Path) -> PathBuf {
                journal_server::journal_file(directory, JOURNAL_SCHEMA)
            }

            /// Host policy whose authority expires, as a worker's does once the
            /// manager leases it rather than the configuration granting it.
            pub fn leased_policy(revision: i64, policy: AppPolicy) -> PolicySnapshot {
                PolicySnapshot::lease(
                    revision.try_into().unwrap(),
                    policy,
                    std::time::Instant::now() + std::time::Duration::from_secs(3600),
                )
                .unwrap()
                .with_ingress_epoch(Some(open_epoch()))
            }

            /// The journal a host opens over a session file in `path`'s
            /// directory, installed.
            ///
            /// Only the directory is read. The journal is kept in
            /// [`journal_file`] of it, which is the path a case that installs,
            /// reads or faults the file directly must name.
            pub async fn sqlite_store(path: &Path) -> OrmStore {
                let directory = path.parent().expect("the path has a directory");
                let store = orm_store(&format!(
                    "sqlite:{}",
                    directory.join("orm.sqlite").display()
                ))
                .await;
                schema::initialize_local(&store).await.unwrap();
                store
            }

            /// The journal a host opens over `url`: `HostStorage`, so
            /// `journal_binding()`.
            pub async fn orm_store(url: &str) -> OrmStore {
                HostStorage::new(ConnectionFactory::for_platform_url(url).unwrap())
                    .open()
                    .await
                    .unwrap()
            }

            /// Install the journal into `admin`'s database as the platform
            /// migration does.
            pub async fn install_journal(admin: &$crate::adapters::__private::compio_postgres::Client) {
                let sql = schema::postgres_sql(&SchemaName::new(JOURNAL_SCHEMA).unwrap());
                journal_server::install_journal(admin, JOURNAL_SCHEMA, &sql).await;
            }

            /// Create the journal owner, login and the platform roles it stays
            /// closed to.
            pub async fn ensure_roles(admin: &$crate::adapters::__private::compio_postgres::Client) {
                journal_server::ensure_roles(admin).await;
            }

            /// Grant the journal to the service login as the platform migration
            /// does.
            pub async fn grant_journal(admin: &$crate::adapters::__private::compio_postgres::Client) {
                journal_server::grant_journal(admin, JOURNAL_SCHEMA).await;
            }

            pub struct PostgresFixture {
                /// The journal opened on the case's database as
                /// [`JOURNAL_LOGIN`].
                pub store: OrmStore,
                pub admin_url: String,
                /// The server `admin_url` names, authenticated as
                /// [`JOURNAL_LOGIN`].
                pub journal_url: String,
                /// The shared bare server database this case owns. Declared after
                /// `store` so the case's connections close before the clone is
                /// removed.
                server: JournalServer,
            }

            impl PostgresFixture {
                pub async fn start() -> Self {
                    let sql = schema::postgres_sql(&SchemaName::new(JOURNAL_SCHEMA).unwrap());
                    // Boxed so the server's and the store's connection state
                    // machines stay out of this future: inlined, their layouts
                    // overflow rustc's default query depth in every crate that
                    // expands these adapters.
                    let server = Box::pin(JournalServer::start(JOURNAL_SCHEMA, &sql)).await;
                    let store = Box::pin(orm_store(&server.journal_url)).await;
                    Self {
                        store,
                        admin_url: server.admin_url.clone(),
                        journal_url: server.journal_url.clone(),
                        server,
                    }
                }

                /// The Docker id of the shared server this case's database lives
                /// on.
                #[must_use]
                pub fn container_id(&self) -> &str {
                    self.server.container_id()
                }
            }

            /// A host service with two registered apps, each on an active
            /// deployment.
            pub async fn registered_service(
                store: Rc<OrmStore>,
            ) -> (WorkflowService, AppId, AppId, Deployments) {
                registered_with_deployments(store, Deployments::new().await).await
            }

            pub async fn registered_with_deployments(
                store: Rc<OrmStore>,
                deployments: Deployments,
            ) -> (WorkflowService, AppId, AppId, Deployments) {
                let a = AppId::mint();
                let b = AppId::mint();
                let service = WorkflowService::open(store, Arc::new(HostPolicies::default()))
                    .await
                    .unwrap()
                    .with_deployments(deployments.binding(&[&a, &b]));
                for app in [&a, &b] {
                    service
                        .fixture_register(app, leased_policy(1, AppPolicy::default()))
                        .await
                        .unwrap();
                    service
                        .fixture_register(app, leased_policy(1, AppPolicy::default()))
                        .await
                        .unwrap();
                    deployments
                        .activate(
                            &service,
                            app,
                            &DeployRegistration {
                                id: typed_id::generate("dep"),
                                hash: "a".repeat(64),
                                workflows: ["Example".into(), "Child".into()].into(),
                                schedules: Vec::new(),
                            },
                        )
                        .await
                        .unwrap();
                }
                (service, a, b, deployments)
            }

            /// Count the journal rows of one table that match a filter.
            pub async fn journal_row_count(
                tx: &zeroship_workflow::service::store::Transaction,
                table: &str,
                filter: $crate::adapters::__private::serde_json::Value,
            ) -> usize {
                use $crate::adapters::__private::zeroship_data_orm::orm::Output;
                let collection = tx
                    .database()
                    .collection(&format!("__zeroship_workflow_{table}"))
                    .unwrap();
                let mut counted = 0;
                loop {
                    let Output::Rows(page) = collection
                        .find(
                            filter.clone().into(),
                            $crate::adapters::__private::zeroship_data_orm::value!({
                                "offset": counted,
                                "limit": $crate::adapters::__private::zeroship_data_orm::sql::MAX_ROW_LIMIT,
                                "orderBy": {"id": 1}
                            }),
                        )
                        .await
                        .unwrap()
                    else {
                        panic!("expected journal rows")
                    };
                    if page.is_empty() {
                        return counted;
                    }
                    counted += page.len();
                }
            }

            /// A resolved frontier carrying the given runtime outcomes.
            pub fn execution(
                value: $crate::adapters::__private::serde_json::Value,
            ) -> zeroship_workflow::WorkflowExecution {
                zeroship_workflow::WorkflowExecution::from_runtime_value(
                    $crate::adapters::__private::serde_json::json!({"outcomes": value}),
                )
                .unwrap()
            }
        }
    };
}
