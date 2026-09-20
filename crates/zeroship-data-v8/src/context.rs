//! Worker-thread bindings for V8 database operations.
//!
//! Isolates on a worker thread share the configured ORM connection. Database
//! opening, pooling and concurrent initialization belong to `LocalConnection`.
//! Transaction lanes, descriptors, protection and CDC state belong to the ORM;
//! request and deployment identity are captured by the V8 wrappers. The
//! service's meter lives here and reaches the ORM only as the usage sink each
//! creator dispatch captures, never as ambient state other bindings could see.

#[cfg(test)]
use zeroship_data_orm::backend::BackendHandle;

use std::{cell::RefCell, sync::Arc};
use zeroship_data_orm::{
    connection::{ConnectionFactory, LocalConnection},
    encryption::{ProjectKeySource, SuppliedProjectKeys},
};

#[derive(Default)]
pub(crate) struct ThreadDbContext {
    connection: Option<LocalConnection>,
    app_bindings: Option<Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings>>,
    cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
    supplied_project_keys: Option<Arc<SuppliedProjectKeys>>,
    meter: Option<Arc<zeroship_metering::Meter>>,
}
impl ThreadDbContext {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    /// The process meter creator dispatches on this thread record into.
    pub(crate) fn meter(&self) -> Option<Arc<zeroship_metering::Meter>> {
        self.meter.clone()
    }
    pub(crate) fn set_meter(&mut self, meter: Option<Arc<zeroship_metering::Meter>>) {
        self.meter = meter;
    }
    pub(crate) fn connection(&self) -> Option<LocalConnection> {
        self.connection.clone()
    }
    #[cfg(test)]
    pub(crate) fn backend(&self) -> Option<BackendHandle> {
        self.connection.as_ref().and_then(LocalConnection::backend)
    }
    /// Reinstalling equal configuration preserves the open ORM connection.
    pub(crate) fn install_connection(&mut self, factory: ConnectionFactory) {
        if self
            .connection
            .as_ref()
            .is_none_or(|current| current.factory().identity() != factory.identity())
        {
            self.connection = Some(LocalConnection::new(factory));
        }
    }
    #[cfg(test)]
    pub(crate) fn clear_backend(&mut self) {
        self.connection = self
            .connection
            .as_ref()
            .map(|current| LocalConnection::new(current.factory().clone()));
    }
    #[cfg(test)]
    pub(crate) fn project_keys(&self) -> Option<Arc<SuppliedProjectKeys>> {
        self.supplied_project_keys.clone()
    }
    pub(crate) fn local_key_source(&self) -> ProjectKeySource {
        match &self.supplied_project_keys {
            Some(keys) => ProjectKeySource::supplied(Arc::clone(keys)),
            None => ProjectKeySource::unavailable(),
        }
    }
    /// One binding a trusted host resolved for an app on this thread, by the
    /// DATABASE it names.
    pub(crate) fn app_binding(
        &self,
        app_id: &str,
        deploy_token: &str,
        database: &zeroship_core::DatabaseId,
    ) -> Option<zeroship_data_orm::binding::DbBinding> {
        self.app_bindings
            .as_ref()
            .and_then(|bindings| bindings.binding_for(app_id, deploy_token, database))
    }

    /// Every binding a trusted host resolved for an app on this thread.
    pub(crate) fn app_bindings(
        &self,
        app_id: &str,
        deploy_token: &str,
    ) -> Vec<zeroship_data_orm::binding::DbBinding> {
        self.app_bindings
            .as_ref()
            .map(|bindings| bindings.bindings_for(app_id, deploy_token))
            .unwrap_or_default()
    }
    pub(crate) fn set_app_bindings(
        &mut self,
        bindings: Option<Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings>>,
    ) {
        self.app_bindings = bindings;
    }
    pub(crate) fn set_supplied_project_keys(&mut self, keys: Option<Arc<SuppliedProjectKeys>>) {
        let same = match (&self.supplied_project_keys, &keys) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.connection = self
                .connection
                .as_ref()
                .map(|current| LocalConnection::new(current.factory().clone()));
        }
        self.supplied_project_keys = keys;
    }
    pub(crate) fn sql_registration(&self) -> zeroship_data_orm::sql::registration::SqlRegistration {
        self.connection
            .as_ref()
            .map(|connection| connection.factory().sql_registration().clone())
            .unwrap_or_else(zeroship_data_orm::sql::registration::SqlRegistration::postgres)
    }
    pub(crate) fn connection_identity(
        &self,
    ) -> Option<zeroship_data_orm::connection::ConnectionIdentity> {
        self.connection
            .as_ref()
            .map(|connection| connection.factory().identity())
    }
    pub(crate) fn cdc_relay(&self) -> Option<zeroship_data_orm::cdc::relay::RelayConfig> {
        self.cdc_relay.clone()
    }
    pub(crate) fn set_cdc_relay(
        &mut self,
        relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
    ) {
        self.cdc_relay = relay;
    }
}
thread_local! {
    static THREAD_DB_CTX: RefCell<ThreadDbContext> = RefCell::new(ThreadDbContext::new());
}
pub(crate) fn with<R>(f: impl FnOnce(&ThreadDbContext) -> R) -> R {
    THREAD_DB_CTX.with_borrow(f)
}
pub(crate) fn with_mut<R>(f: impl FnOnce(&mut ThreadDbContext) -> R) -> R {
    THREAD_DB_CTX.with_borrow_mut(f)
}
pub(crate) fn isolate_key_source() -> ProjectKeySource {
    with(ThreadDbContext::local_key_source)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_replacement_does_not_open_a_database() {
        let mut context = ThreadDbContext::new();
        assert!(context.connection().is_none());
        let first = ConnectionFactory::for_app_url("postgres://first/db").unwrap();
        context.install_connection(first.clone());
        assert_eq!(
            context.connection().unwrap().factory().identity(),
            first.identity()
        );
        assert!(context.backend().is_none());
        let second = ConnectionFactory::for_app_url("sqlite:context.sqlite").unwrap();
        context.install_connection(second.clone());
        assert_eq!(
            context.connection().unwrap().factory().identity(),
            second.identity()
        );
        assert!(context.backend().is_none());
    }
    #[compio::test]
    async fn a_pending_open_cannot_replace_a_new_thread_binding() {
        use futures::FutureExt;
        use zeroship_data_orm::{connection::BackendFactory, error::DbError};
        struct DelayedFactory(ConnectionFactory, flume::Receiver<()>);
        impl BackendFactory for DelayedFactory {
            fn sql_registration(&self) -> zeroship_data_orm::sql::registration::SqlRegistration {
                self.0.sql_registration().clone()
            }
            fn connect(
                &self,
                keys: ProjectKeySource,
            ) -> futures::future::LocalBoxFuture<'_, Result<BackendHandle, DbError>> {
                Box::pin(async move {
                    self.1.recv_async().await.expect("release old open");
                    self.0.connect(keys).await
                })
            }
        }
        crate::tests::fixtures::reset_context();
        let directory = tempfile::tempdir().unwrap();
        let old = ConnectionFactory::for_app_url(&format!(
            "sqlite:{}",
            directory.path().join("old.sqlite").display()
        ))
        .unwrap();
        let (release, receiver) = flume::bounded(1);
        let old = ConnectionFactory::new("delayed_old", DelayedFactory(old, receiver));
        with_mut(|context| context.install_connection(old));
        let mut opening = crate::tx_scope::ensure_backend().boxed_local();
        assert!(futures::poll!(&mut opening).is_pending());
        let next = ConnectionFactory::for_app_url(&format!(
            "sqlite:{}",
            directory.path().join("new.sqlite").display()
        ))
        .unwrap();
        with_mut(|context| context.install_connection(next.clone()));
        release.send(()).unwrap();
        opening.await.unwrap();
        assert!(with(|context| context.backend()).is_none());
        assert_eq!(
            with(|context| context.connection().unwrap().factory().identity()),
            next.identity()
        );
        crate::tx_scope::ensure_backend().await.unwrap();
        let opened = zeroship_data_orm::connection::backend_open_count();
        with_mut(|context| context.install_connection(next));
        crate::tx_scope::ensure_backend().await.unwrap();
        assert_eq!(zeroship_data_orm::connection::backend_open_count(), opened);
        assert!(directory.path().join("old.sqlite").exists());
        assert!(directory.path().join("new.sqlite").exists());
        crate::tests::fixtures::reset_context();
    }
}
