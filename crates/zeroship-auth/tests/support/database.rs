//! The migrated platform database an auth case runs against.
//!
//! Every case in this binary shares one reaper-owned `PostgreSQL` server through
//! [`zeroship_testkit::postgres::platform`], migrated once. A case that owns
//! only the rows and names it mints runs on that shared database; a case whose
//! subject is platform-global - the signing-key registry, an audit or token
//! sweep that acts on every row, a schema-level race - gets its own database
//! cloned from the connection-free migrated template with
//! [`Database::run_fresh`].
//!
//! Connections the case opens are joined before its runtime ends, including
//! when an assertion unwinds, and the case's own panic is re-raised after that.
//! The runner lives in the testkit; this is the auth-specific adapter over it.

use compio_postgres::Client;
use std::time::Duration;

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
    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn run_fresh(test: impl AsyncFnOnce(&Self)) {
        zeroship_testkit::postgres::run_fresh::<Self>(test).await;
    }

    pub fn url(&self) -> &str {
        self.base_url().as_str()
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

    /// Connect as `role`, whose password is its own name, for a case that mints
    /// a role to scope the connections it observes.
    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn connect_as(&self, role: &str) -> Client {
        let mut url = self.base_url().clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        self.connect_to(url.as_str()).await
    }

    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn orm(&self) -> zeroship_data_orm::Database {
        zeroship_auth::store::native::connect(self.auth_url().as_str())
            .await
            .expect("connect the native auth repository")
    }

    /// An ORM pool connecting as `role`, whose password is its own name, for a
    /// case that needs a reader with different role attributes than the shared
    /// auth role.
    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn orm_as(&self, role: &str) -> zeroship_data_orm::Database {
        let mut url = self.base_url().clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        zeroship_auth::store::native::connect(url.as_str())
            .await
            .expect("connect the native auth repository")
    }

    /// The auth role's URL under a session name that fixture probes and
    /// triggers can match through `application_name`.
    pub fn auth_url_named(&self, application_name: &str) -> url::Url {
        let mut url = self.auth_url();
        url.query_pairs_mut()
            .append_pair("application_name", application_name);
        url
    }
}

/// Poll server state until `holds` reports true, or report that it never did.
///
/// The bound stays inside the ORM's `DB_LOCK_TIMEOUT_MS`, so a repository
/// statement the fixture parked on a lock is still waiting when the probe
/// gives up and the case can release it before asserting.
#[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
pub async fn eventually(mut holds: impl AsyncFnMut() -> bool) -> bool {
    let bound =
        Duration::from_millis(u64::from(zeroship_data_orm::budgets::DB_LOCK_TIMEOUT_MS) / 2);
    compio::time::timeout(bound, async {
        while !holds().await {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}
