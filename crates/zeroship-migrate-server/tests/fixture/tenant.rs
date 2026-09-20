//! A throwaway tenant cluster, owned by one test.
//!
//! THE SERVER IS PER TEST AND THAT IS NOT A CONVENIENCE. `pg_authid` and
//! `pg_auth_members` are cluster-shared, so every role this reconciler creates
//! or reaps would be visible to - and reapable by - a sibling test on the same
//! instance. A reap arm in particular measures which roles are GONE, and a
//! shared cluster would make that a statement about whichever test ran last.

use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

/// The major the platform deploys (`deploy/compose/docker-compose.yml`), and
/// the floor the tenant fence exists on.
pub const DEPLOY_MAJOR: &str = "16";

/// A major BELOW the floor, used to exhibit the bootstrap's version refusal.
/// `pg_auth_members` carries no `inherit_option` here at all.
pub const PRE_FENCE_IMAGE: (&str, &str) = ("postgres", "15-alpine");

pub struct Cluster {
    _container: Container<GenericImage>,
    url: String,
}

impl Cluster {
    /// A cluster on the deployed major.
    pub fn start() -> Self {
        Self::of("postgres", DEPLOY_MAJOR)
    }

    /// A cluster on a named image, for the arms that are about the version.
    pub fn of(image: &str, tag: &str) -> Self {
        let container = GenericImage::new(image, tag)
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "tenant_cluster")
            .with_cmd(["postgres", "-c", "fsync=off"])
            .with_startup_timeout(Duration::from_secs(180))
            .start()
            .unwrap_or_else(|error| {
                panic!(
                    "REFUSED: this test requires Docker and a {image}:{tag} cluster, and it is \
                     not there.\n\
                     \n\
                     \x20   problem   {error}\n\
                     \n\
                     \x20   NO VERDICT WAS REACHABLE. The reconciler never ran, so this\n\
                     \x20   failure says nothing about the code.\n\
                     \n\
                     \x20   Start Docker and re-run. There is no environment variable that\n\
                     \x20   makes this a skip."
                )
            });
        let host = container.get_host().expect("tenant cluster host");
        let port = container
            .get_host_port_ipv4(5432)
            .expect("tenant cluster port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/tenant_cluster");
        Self {
            _container: container,
            url,
        }
    }

    /// The privileged DSN the migration service would hold for this cluster.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The same cluster, reached as one of its own login roles.
    pub fn url_as(&self, role: &str, password: &str) -> String {
        let mut url = url::Url::parse(&self.url).expect("the fixture DSN parses");
        url.set_username(role).expect("the DSN accepts a username");
        url.set_password(Some(password))
            .expect("the DSN accepts a password");
        url.into()
    }
}
