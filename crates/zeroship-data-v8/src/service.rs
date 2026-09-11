//! Process-wide configuration for the V8 database primitive.
//! Hosts supply an ORM connection factory; isolates clone the plugin prototype.
use crate::DbPlugin;
use std::sync::Arc;
use zeroship_data_orm::{connection::ConnectionFactory, error::DbError};

#[derive(Debug, Clone)]
pub struct DbServiceConfig {
    pub connection: ConnectionFactory,
    pub cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
    pub meter: Option<Arc<zeroship_metering::Meter>>,
}

#[derive(Debug)]
pub struct DbService {
    plugin: Arc<DbPlugin>,
}
impl DbService {
    pub fn new(config: DbServiceConfig) -> Result<Arc<Self>, DbError> {
        let assignments = zeroship_data_orm::system_shape_charter::AssignmentPlan::load()?;
        Ok(Arc::new(Self {
            plugin: Arc::new(DbPlugin::new(
                config.connection,
                config.cdc_relay,
                config.meter,
                assignments,
            )),
        }))
    }
    pub fn plugin(&self) -> Arc<DbPlugin> {
        Arc::clone(&self.plugin)
    }
    pub fn connection(&self) -> &ConnectionFactory {
        &self.plugin.connection
    }
    pub fn lifecycle(&self) -> DbLifecycle {
        DbLifecycle
    }
}

/// Host teardown for the adapter's local subscriptions.
#[derive(Debug, Clone, Copy)]
pub struct DbLifecycle;
impl DbLifecycle {
    pub async fn deprovision_app(&self, app_id: &str) -> Result<(), DbError> {
        zeroship_data_orm::cdc::lifecycle::shutdown_app(app_id).await;
        zeroship_data_orm::cdc::broker::drop_app(app_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[compio::test]
    async fn app_teardown_needs_no_database_connection() {
        let connection =
            ConnectionFactory::for_url("postgres://unused:unused@127.0.0.1:1/unused").unwrap();
        let service = DbService::new(DbServiceConfig {
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
            connection: ConnectionFactory::for_url("postgres://user:secret@host/db").unwrap(),
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
