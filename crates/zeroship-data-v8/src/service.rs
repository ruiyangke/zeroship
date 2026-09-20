//! Process-wide configuration for the V8 database primitive.
//! Hosts supply an ORM connection factory; isolates clone the plugin prototype.
use crate::DbPlugin;
use std::sync::Arc;
use zeroship_data_orm::{connection::ConnectionFactory, error::DbError};

#[derive(Debug, Clone)]
pub struct DbServiceConfig {
    pub connection: ConnectionFactory,
    /// Project key material delivered by the trusted host.
    pub project_keys: Arc<zeroship_data_orm::encryption::SuppliedProjectKeys>,
    /// The app-to-database bindings the trusted host resolved.
    ///
    /// An isolate cannot compose one: the database id, the edge id and the
    /// schema epoch are control-plane facts and the role the session narrows to
    /// is derived from two of them. An app with nothing here has no `env.db`.
    pub app_bindings: Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings>,
    pub cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
    /// Records each creator dispatch's database usage under its app id. With a
    /// meter, a binding whose app id is not an app id is refused.
    pub meter: Option<Arc<zeroship_metering::Meter>>,
}

#[derive(Debug)]
pub struct DbService {
    plugin: Arc<DbPlugin>,
}
impl DbService {
    pub fn new(config: DbServiceConfig) -> Result<Arc<Self>, DbError> {
        Ok(Arc::new(Self {
            plugin: Arc::new(DbPlugin::new(
                config.connection,
                config.cdc_relay,
                config.meter,
                config.project_keys,
                config.app_bindings,
            )),
        }))
    }
    pub fn plugin(&self) -> Arc<DbPlugin> {
        Arc::clone(&self.plugin)
    }
    pub fn project_keys(&self) -> &Arc<zeroship_data_orm::encryption::SuppliedProjectKeys> {
        &self.plugin.project_keys
    }
    /// The store a trusted host installs each app's resolved binding into.
    pub fn app_bindings(
        &self,
    ) -> &Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings> {
        &self.plugin.app_bindings
    }
    pub fn connection(&self) -> &ConnectionFactory {
        &self.plugin.connection
    }
    pub fn lifecycle(&self) -> DbLifecycle {
        DbLifecycle {
            keys: self.project_keys().clone(),
            app_bindings: self.app_bindings().clone(),
        }
    }
}

/// Host teardown for the adapter's local subscriptions.
#[derive(Debug, Clone)]
pub struct DbLifecycle {
    keys: Arc<zeroship_data_orm::encryption::SuppliedProjectKeys>,
    app_bindings: Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings>,
}
impl DbLifecycle {
    pub async fn deprovision_app(&self, app_id: &str) -> Result<(), DbError> {
        zeroship_data_orm::cdc::lifecycle::shutdown_app(app_id).await;
        zeroship_data_orm::cdc::broker::drop_app(app_id);
        self.keys.remove_app(app_id)?;
        // The binding goes with the app: a redeploy of a name that was deleted
        // must resolve afresh rather than narrow to the retired edge.
        self.app_bindings.remove_app(app_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[compio::test]
    async fn app_teardown_needs_no_database_connection() {
        let connection =
            ConnectionFactory::for_app_url("postgres://unused:unused@127.0.0.1:1/unused").unwrap();
        let service = DbService::new(DbServiceConfig {
            app_bindings: Default::default(),
            project_keys: Default::default(),
            connection,
            cdc_relay: None,
            meter: None,
        })
        .unwrap();
        let before = zeroship_data_orm::connection::backend_open_count();
        let app = "app_teardown_without_driver";
        let lease = zeroship_data_orm::cdc::lifecycle::acquire(app);
        service.lifecycle().deprovision_app(app).await.unwrap();
        drop(lease);
        assert_eq!(zeroship_data_orm::connection::backend_open_count(), before);
    }
    #[test]
    fn service_clones_the_plugin_and_redacts_configuration() {
        let config = DbServiceConfig {
            app_bindings: Default::default(),
            project_keys: Default::default(),
            connection: ConnectionFactory::for_app_url("postgres://user:secret@host/db").unwrap(),
            cdc_relay: None,
            meter: None,
        };
        assert!(!format!("{config:?}").contains("secret"));
        let service = DbService::new(config).unwrap();
        assert!(Arc::ptr_eq(&service.plugin(), &service.plugin()));
        assert!(!format!("{service:?}").contains("secret"));
        fn thread_safe<T: Send + Sync>() {}
        thread_safe::<DbService>();
    }
}
