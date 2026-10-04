//! The PostgreSQL servers the migration server's integration tests run against.
//!
//! [`Postgres::start`] hands a test a server of its own, a testcontainers
//! `Container` the test holds and drops when it ends. [`migrated_url`] is the
//! migrated control database every test process of the worktree shares, on a
//! platform server of this suite's own scope: its cases bootstrap cluster roles
//! and declare execution zones, which the other suites sharing the worktree's
//! platform server must never observe. The process holds that server's lease
//! for as long as it runs, and the server's watchdog removes it once no process
//! has held the lease for the idle grace, which
//! `datastore_reconciler_pg::migrated_server_lifetime` measures.

use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use zeroship_testkit::postgres::{Platform, PLATFORM_IDLE_GRACE};
use zeroship_testkit::shared::Scope;

pub use zeroship_testkit::tenant_cluster as tenant;
pub mod world;

/// The scope kind this suite's migrated server is filed under.
const MIGRATED_KIND: &str = "migrate-server-platform";

pub struct Postgres {
    _owned: Container<GenericImage>,
    url: String,
}

impl Postgres {
    pub fn start() -> Self {
        let owned = GenericImage::new("postgres", "17")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "migrate_server_tests")
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .unwrap_or_else(|error| {
                panic!("migration-server tests require Docker and PostgreSQL: {error}")
            });
        let host = owned.get_host().expect("database host");
        let port = owned.get_host_port_ipv4(5432).expect("database port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/migrate_server_tests");
        Self {
            _owned: owned,
            url,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Join this suite's migrated server at `scope`, booting and migrating it if
/// this process is elected.
///
/// # Errors
/// When the server cannot be booted, migrated or joined.
pub fn join_migrated(scope: &Scope) -> Result<Platform, String> {
    Platform::join(scope)
}

/// The superuser DSN of the migrated control database this suite shares.
pub fn migrated_url() -> String {
    migrated().admin_url().to_string()
}

/// This suite's migrated server, joined on first use and leased for the life of
/// the process.
pub fn migrated() -> &'static Platform {
    static MIGRATED: OnceLock<Platform> = OnceLock::new();
    MIGRATED.get_or_init(|| {
        join_migrated(&Scope::worktree_with_grace(MIGRATED_KIND, PLATFORM_IDLE_GRACE))
            .unwrap_or_else(|error| {
                panic!("the migration server's migrated control database could not start: {error}")
            })
    })
}
