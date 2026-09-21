//! Binding an app's policy generation the way a trusted host would.
//!
//! Both halves of the workflow engine open apps this way in their tests, so the
//! extension trait is shared: `WorkflowService` is foreign to one of them and an
//! inherent `impl` on it does not compile there.

#![allow(
    dead_code,
    reason = "fixture consumers open apps at different points of the lifecycle"
)]

use std::{future::Future, sync::Arc};
use zeroship_core::app_id::AppId;
use zeroship_workflow::{
    service::{AppWorkflows, HostPolicies, PolicyBinding, PolicySnapshot, WorkflowService},
    WorkflowServiceError,
};

pub trait ServiceFixture {
    /// The app's live binding, created when the host has not bound it yet.
    fn fixture_binding(&self, app: &AppId) -> Result<PolicyBinding, WorkflowServiceError>;

    /// Open the app on its live binding.
    fn fixture_app(&self, app: AppId) -> AppWorkflows;

    /// Install policy and register the app, as a host does on first contact.
    fn fixture_register(
        &self,
        app: &AppId,
        snapshot: PolicySnapshot,
    ) -> impl Future<Output = Result<(), WorkflowServiceError>>;

    /// Install over the existing binding without re-registering the app, so a
    /// fixture can reissue policy after its setup already ran.
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

    #[expect(
        clippy::future_not_send,
        reason = "fixture bindings own compio-local journals"
    )]
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
