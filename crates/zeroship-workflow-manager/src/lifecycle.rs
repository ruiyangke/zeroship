//! Control's terminal app lifecycle as the manager observes it.
//!
//! Deletion is terminal: an app never returns from it. The closing lane asks
//! which of its candidates Control deleted and abandons their responsibility
//! instead of closing it.
#![expect(
    clippy::future_not_send,
    reason = "lifecycle reads stay on the manager's owning compio runtime"
)]

use crate::Error;
use std::{collections::BTreeSet, fmt::Debug, future::Future, pin::Pin};
use zeroship_core::app_id::AppId;
use zeroship_data_orm::{
    orm::{Database, Entity, FromRow},
    schema::Schema,
};

/// Reports the apps whose terminal deletion Control recorded.
pub trait AppLifecycle: Debug {
    /// The subset of `apps` Control deleted. An app absent from Control's
    /// catalog is not reported deleted; only a recorded deletion abandons.
    fn deleted<'a>(
        &'a self,
        apps: &'a [AppId],
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<AppId>, Error>> + 'a>>;
}

/// A host whose apps cannot be deleted: the local development host is its
/// app's only platform authority and keeps no Control catalog.
#[derive(Debug, Default, Clone, Copy)]
pub struct Undeletable;

impl AppLifecycle for Undeletable {
    fn deleted<'a>(
        &'a self,
        _: &'a [AppId],
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<AppId>, Error>> + 'a>> {
        Box::pin(async { Ok(BTreeSet::new()) })
    }
}

zeroship_data_orm::orm::schema! {
    source {
        apps {
            #[orm(primary_key)]
            id: Text,
            deleted_at: Nullable<Timestamp>,
        }
    }
}
use source::apps;

/// Native metadata for a host-provisioned Control database binding. The reader
/// selects only app identity and the deletion marker.
///
/// # Errors
/// Refuses invalid native model declarations.
pub fn collections() -> Result<Schema, Error> {
    let schema = Schema::new(vec![(
        apps::Entity::COLLECTION.into(),
        apps::Entity::schema().clone(),
    )]);
    schema.validate()?;
    Ok(schema)
}

#[derive(FromRow)]
#[orm(entity = apps)]
struct Deleted {
    id: String,
}

/// Control's app catalog, read through the manager's column grants. It opens
/// no creator database and writes nothing.
#[derive(Debug, Clone)]
pub struct ControlLifecycle {
    database: Database,
}

impl ControlLifecycle {
    /// Bind a Control database provisioned and authorized by the platform host.
    ///
    /// # Errors
    /// Refuses missing or incompatible native collection metadata.
    pub fn new(database: Database) -> Result<Self, Error> {
        database.entity::<apps::Entity>()?;
        Ok(Self { database })
    }

    /// Verify the deletion marker is readable before the driver relies on it.
    ///
    /// # Errors
    /// Refuses unavailable or unauthorized source storage.
    pub async fn ready(&self) -> Result<(), Error> {
        self.database
            .entity::<apps::Entity>()?
            .query()
            .filter(apps::deleted_at.is_not_null())
            .limit(1)?
            .all::<Deleted>()
            .await
            .map(|_| ())
            .map_err(|_| Error::Unavailable)
    }
}

impl AppLifecycle for ControlLifecycle {
    fn deleted<'a>(
        &'a self,
        apps: &'a [AppId],
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<AppId>, Error>> + 'a>> {
        Box::pin(async move {
            if apps.is_empty() {
                return Ok(BTreeSet::new());
            }
            let limit = i64::try_from(apps.len()).map_err(|_| Error::Capacity)?;
            let rows = self
                .database
                .entity::<apps::Entity>()?
                .query()
                .filter(
                    apps::id
                        .in_values(apps.iter().map(AppId::as_str))?
                        .and(apps::deleted_at.is_not_null()),
                )
                .limit(limit)?
                .all::<Deleted>()
                .await?;
            rows.into_iter()
                .map(|row| {
                    let app = AppId::parse(&row.id).map_err(|_| Error::Storage)?;
                    if apps.contains(&app) {
                        Ok(app)
                    } else {
                        Err(Error::Storage)
                    }
                })
                .collect()
        })
    }
}
