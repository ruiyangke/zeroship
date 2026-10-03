//! The migrated platform database this crate's unit tests run against.
//!
//! Unit tests exercise internals their own module does not export, so they
//! cannot live in the integration target. They share the same reaper-owned
//! server as every other test binary through [`zeroship_testkit::postgres`]:
//! [`Database::run`] uses the shared migrated database, and a case scopes
//! itself to the rows it mints.
//!
//! Connections the case opens are joined before its runtime ends, including
//! when an assertion unwinds, and the case's own panic is re-raised after that.
//! The runner lives in the testkit; this is the auth-specific adapter over it.

use compio_postgres::Client;
use zeroship_testkit::postgres::{Case, CaseFixture};

pub struct Database {
    case: Case,
}

impl CaseFixture for Database {
    fn from_case(case: Case) -> Self {
        Self { case }
    }

    fn case(&self) -> &Case {
        &self.case
    }
}

impl std::ops::Deref for Database {
    type Target = Case;

    fn deref(&self) -> &Self::Target {
        &self.case
    }
}

impl Database {
    /// Run `test` against the shared migrated database.
    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        zeroship_testkit::postgres::run::<Self>(test).await;
    }

    /// Run `test` against a database cloned from the migrated template.
    ///
    /// A case whose subject is platform-global rather than scoped to the rows
    /// it mints - here the `signing_keys` registry, where publishing one key
    /// retires every other active key - uses this so no other process can reach
    /// the rows it owns.
    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn run_fresh(test: impl AsyncFnOnce(&Self)) {
        zeroship_testkit::postgres::run_fresh::<Self>(test).await;
    }

    pub fn auth_url(&self) -> url::Url {
        let mut url = self.base_url().clone();
        url.set_username("zeroship_auth").unwrap();
        url.set_password(Some("zeroship_auth")).unwrap();
        url
    }

    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn connect_as_auth(&self) -> Client {
        self.connect_to(self.auth_url().as_str()).await
    }

    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn orm(&self) -> zeroship_data_orm::Database {
        crate::store::native::connect(self.auth_url().as_str())
            .await
            .expect("connect the native auth repository")
    }
}
