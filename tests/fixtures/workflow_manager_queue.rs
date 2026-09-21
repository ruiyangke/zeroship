//! A manager job queue on its own SQLite file, with retention stubbed out.
//!
//! Publication and delivery contracts need a real manager queue but not a real
//! artifact catalog: the manager's retention suite owns deployment safety.

#![allow(
    dead_code,
    reason = "fixture consumers exercise different queue contracts"
)]
#![expect(
    clippy::future_not_send,
    reason = "fixtures use their owning compio thread"
)]

use std::{path::Path, rc::Rc};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::DeploymentId,
};
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
    retention::{HoldClient, HoldFuture},
    Options, Queue,
};

/// Confirms every hold without a catalog, so a queue contract turns on the
/// queue rather than on artifact retention.
#[derive(Debug)]
struct StubbedHolds;

impl StubbedHolds {
    fn receipt(
        app: &AppId,
        deployment: &DeploymentId,
        generation: HoldGeneration,
        state: HoldState,
    ) -> HoldReceipt {
        HoldReceipt {
            app_id: app.clone(),
            deploy_id: deployment.as_str().into(),
            deploy_hash: zeroship_bundle::sha256_hex(deployment.as_str().as_bytes()),
            holder_id: HoldScope::for_queue(app.clone()).holder().into(),
            generation,
            state,
        }
    }
}

impl HoldClient for StubbedHolds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move { Ok(Self::receipt(app, deployment, generation, HoldState::Held)) })
    }

    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            Ok(Self::receipt(
                app,
                deployment,
                generation,
                HoldState::Released,
            ))
        })
    }
}

pub struct Manager {
    _directory: tempfile::TempDir,
    pub path: std::path::PathBuf,
    pub queue: Queue,
}
impl Manager {
    pub async fn new(app: &AppId) -> Self {
        Self::with_options(app, Options::default()).await
    }
    pub async fn with_options(app: &AppId, options: Options) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manager.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")
            .unwrap();
        connection
            .execute_batch(include_str!(
                "../../crates/zeroship-workflow-manager/schema/sqlite.sql"
            ))
            .unwrap();
        let queue = Self::open_with_options(&path, options).await;
        queue.register_scope(app).await.unwrap();
        Self {
            _directory: directory,
            path,
            queue,
        }
    }
    pub async fn open(path: &Path) -> Queue {
        Self::open_with_options(path, Options::default()).await
    }
    pub async fn open_with_options(path: &Path, options: Options) -> Queue {
        Queue::connect(
            DbBinding::platform(
                "workflow_manager",
                "publication-test",
                SchemaName::new("main").unwrap(),
            ),
            &format!("sqlite:{}", path.display()),
            options,
            Rc::new(StubbedHolds),
        )
        .await
        .unwrap()
    }
    pub fn count(&self) -> i64 {
        rusqlite::Connection::open(&self.path)
            .unwrap()
            .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
            .unwrap()
    }
}

/// A manager-issued epoch for fixtures whose acceptance needs open responsibility.
pub fn open_epoch() -> zeroship_core::workflow_coordination::Revision {
    1.try_into().unwrap()
}
