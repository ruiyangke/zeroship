//! The migrated platform database a mailer case runs against.
//!
//! Every case in this binary shares one reaper-owned `PostgreSQL` server through
//! [`zeroship_testkit::postgres`], migrated once. A case that owns only the rows
//! it mints runs on that shared database with [`Database::run`]; a case whose
//! subject is platform-global - here renaming the shared suppression table, so
//! the lookup fails for every reader - gets its own database cloned from the
//! connection-free migrated template with [`Database::run_fresh`].
//!
//! Connections the case opens are joined before its runtime ends, including
//! when an assertion unwinds, and the case's own panic is re-raised after that.
//! The runner lives in the testkit; this is the mailer-specific adapter over it.

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
    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn run_fresh(test: impl AsyncFnOnce(&Self)) {
        zeroship_testkit::postgres::run_fresh::<Self>(test).await;
    }

    /// Connect as `role`, whose password is its own name, for a case that
    /// exercises the role-scoped mailer path.
    #[expect(clippy::future_not_send, reason = "fixtures belong to their compio runtime")]
    pub async fn connect_as(&self, role: &str) -> Client {
        let mut url = self.base_url().clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        self.connect_to(url.as_str()).await
    }
}