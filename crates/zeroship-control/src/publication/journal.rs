//! Keeping a deploying app's workflow journal current.
//!
//! # Why the deploy path asks, and why it asks the manager
//!
//! The journal is a platform-owned schema inside the creator's database, and
//! nothing in the running system can create one: the worker executes creator
//! code and holds no DDL authority. Until a deploy asked for it, a journal
//! appeared only after a host had already REFUSED to use one - so the first
//! workflow run of a newly deployed app paid a refusal-and-repair round trip,
//! and an app that was deployed but never run had no journal at all.
//!
//! Control learns that an app deployed, so Control is what asks. It asks the
//! MANAGER rather than the migration service, because the manager owns the
//! journal artifacts; routing the bundle through Control would move those
//! artifacts into the control plane, which is the leak the bundle path exists
//! to remove.
//!
//! # No "already provisioned" flag
//!
//! The manager's ensure is idempotent by design - it reads the installed stamp
//! and installs, upgrades, verifies or refuses - so the deploy path calls it
//! every time. A Control-side "have we provisioned this app" flag would be a
//! second source of truth for a fact the journal already carries, and it would
//! go stale exactly when the schema version moves.

use std::{rc::Rc, sync::Arc};

use zeroship_core::{
    app_derivation, app_id::AppId, schema_bundle::SchemaBundleOutcome, schema_name::SchemaName,
    service_peers::ServiceAuth,
};
use zeroship_workflow_client::{ControlCoordinator, Error as ManagerError, Options};

use super::publisher::Exchange;

/// The manager's journal-ensure route. The production implementation is
/// [`ControlCoordinator`], whose call validates that the outcome describes the
/// schema it asked about before returning it.
pub trait JournalManager {
    fn ensure<'a>(&'a self, schema: &'a str) -> Exchange<'a, SchemaBundleOutcome>;
}

impl JournalManager for ControlCoordinator {
    fn ensure<'a>(&'a self, schema: &'a str) -> Exchange<'a, SchemaBundleOutcome> {
        Box::pin(self.ensure_journal(schema))
    }
}

/// Why a deploy could not leave the app's journal current.
///
/// Every variant fails the deploy. A deploy that answered success over a
/// journal that was never installed would hand the creator an app whose first
/// workflow run fails, with nothing in the response saying so.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("the app's creator schema name is not a usable schema identifier")]
    Schema,
    #[error("the workflow manager did not provision the app's journal: {0}")]
    Manager(#[from] ManagerError),
}

/// Control's half of journal provisioning: resolve the app's creator schema and
/// ask the manager to bring its journal to the current version.
#[derive(Clone)]
pub struct DeployJournal {
    manager: Rc<dyn JournalManager>,
}

impl std::fmt::Debug for DeployJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeployJournal").finish_non_exhaustive()
    }
}

impl DeployJournal {
    /// Bind the workflow manager's origin and Control's own service signer.
    ///
    /// The HTTP client's pooled streams belong to the thread that opens them,
    /// so this is built per serving thread rather than shared.
    ///
    /// # Errors
    /// Refuses an origin the manager client refuses and a process that carries
    /// no Control service signer.
    pub fn connect(url: &str, auth: Arc<ServiceAuth>) -> Result<Self, ManagerError> {
        Ok(Self::new(Rc::new(ControlCoordinator::new(
            url,
            auth,
            Options::default(),
        )?)))
    }

    /// Compose over an arbitrary manager route.
    #[must_use]
    pub fn new(manager: Rc<dyn JournalManager>) -> Self {
        Self { manager }
    }

    /// Ensure this app's creator database carries the current journal.
    ///
    /// # Errors
    /// Refuses an unusable schema name and reports every refusal the manager
    /// answers with, including a journal NEWER than the platform's - which is a
    /// real refusal rather than a failure to try, because an older platform
    /// must never downgrade a creator's journal.
    pub async fn ensure(&self, app: &AppId) -> Result<SchemaBundleOutcome, JournalError> {
        let schema = Self::schema_for(app)?;
        self.manager
            .ensure(schema.as_str())
            .await
            .map_err(JournalError::Manager)
    }

    /// The physical schema this app's journal lives in.
    ///
    /// It goes through [`app_derivation::schema_name`], the one hoisted answer
    /// the data plane, the worker's repair path and the migration service all
    /// read, so Control cannot answer the question differently from the party
    /// that will use the journal. When the creator database is promoted from
    /// app level to project level, that function is the site that changes, and
    /// this call moves with it.
    ///
    /// # Errors
    /// Refuses a derived name that is not a legal schema identifier.
    fn schema_for(app: &AppId) -> Result<SchemaName, JournalError> {
        SchemaName::new(&app_derivation::schema_name(app)).map_err(|_| JournalError::Schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records what it was asked, and answers whatever the test wants.
    struct Recorder {
        asked: RefCell<Vec<String>>,
        answer: Result<SchemaBundleOutcome, ManagerError>,
    }

    impl JournalManager for Recorder {
        fn ensure<'a>(&'a self, schema: &'a str) -> Exchange<'a, SchemaBundleOutcome> {
            self.asked.borrow_mut().push(schema.to_owned());
            let answer = self.answer.clone();
            Box::pin(async move { answer })
        }
    }

    fn outcome(schema: &str) -> SchemaBundleOutcome {
        SchemaBundleOutcome {
            schema: schema.to_owned(),
            bundle: "workflow_journal".to_owned(),
            version: 1,
            action: zeroship_core::schema_bundle::SchemaBundleAction::Installed,
        }
    }

    /// The schema Control names must be the one the worker's own repair path
    /// names for the same app. Two answers to that question would provision a
    /// journal in a schema nothing reads.
    #[compio::test]
    async fn the_schema_asked_about_is_the_platform_derivation() {
        let app = AppId::mint();
        let recorder = Rc::new(Recorder {
            asked: RefCell::new(Vec::new()),
            answer: Ok(outcome(&app_derivation::schema_name(&app))),
        });
        DeployJournal::new(recorder.clone())
            .ensure(&app)
            .await
            .expect("the recorder answers");
        assert_eq!(
            recorder.asked.borrow().as_slice(),
            &[app_derivation::schema_name(&app)]
        );
    }

    /// A manager refusal must reach the caller. Swallowing it is what would let
    /// a deploy report success over a journal that is not there.
    #[compio::test]
    async fn a_manager_refusal_is_reported_rather_than_swallowed() {
        let app = AppId::mint();
        let error = DeployJournal::new(Rc::new(Recorder {
            asked: RefCell::new(Vec::new()),
            answer: Err(ManagerError::Unavailable),
        }))
        .ensure(&app)
        .await
        .expect_err("an unavailable manager must not look like a provisioned journal");
        assert!(matches!(
            error,
            JournalError::Manager(ManagerError::Unavailable)
        ));
    }
}
