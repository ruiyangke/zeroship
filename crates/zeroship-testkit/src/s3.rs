//! An owned S3-compatible server with Docker-assigned ports.
//!
//! The server is the Versity S3 Gateway over its POSIX backend: a real S3
//! implementation that verifies `SigV4` (an unsigned or wrongly-signed request is
//! refused), assembles genuine multipart uploads and answers ranged GETs, so a
//! signing or multipart regression fails here instead of passing against a mock
//! that accepts anything.
//!
//! The container is a testcontainers `Container` handle: dropping it removes the
//! gateway, and the crate's process watchdog removes it if the owning process is
//! signalled before the drop runs.

use std::time::Duration;

use testcontainers::{
    core::{IntoContainerPort, Mount, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

const IMAGE: &str = "ghcr.io/versity/versitygw";
/// Pinned: a floating tag is how a fixture silently changes servers under a
/// green suite, or stops resolving when a tag is withdrawn.
const TAG: &str = "v1.3.0";
const ACCESS: &str = "zeroship-fixture";
const SECRET: &str = "zeroship-fixture-secret";
const BUCKET: &str = "storage-fixture";
const PORT: u16 = 7070;
const DATA_CAPACITY: i64 = 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct S3Server {
    _container: Container<GenericImage>,
    endpoint: String,
}

impl S3Server {
    pub fn start() -> Self {
        // The POSIX backend maps each bucket to a directory under the gateway
        // root, so the bucket exists as soon as the directory does: no client
        // and no vendor CLI take part in fixture setup.
        let boot = format!(
            "mkdir -p /data/{BUCKET} && exec /usr/local/bin/versitygw --port :{PORT} posix /data"
        );
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(PORT.tcp())
            // The gateway prints its banner once the listener is bound.
            .with_wait_for(WaitFor::message_on_stdout("VersityGW"))
            .with_entrypoint("/bin/sh")
            .with_env_var("ROOT_ACCESS_KEY_ID", ACCESS)
            .with_env_var("ROOT_SECRET_ACCESS_KEY", SECRET)
            // Disposable fixture data must not consume the host's build-cache
            // space.
            .with_mount(Mount::tmpfs_mount("/data").with_size_bytes(DATA_CAPACITY))
            .with_cmd(["-c".to_string(), boot])
            .with_startup_timeout(Duration::from_secs(90))
            .start()
            .expect("S3 tests require Docker");
        let endpoint = format!(
            "http://{}:{}",
            container.get_host().expect("S3 fixture host"),
            container
                .get_host_port_ipv4(PORT.tcp())
                .expect("S3 fixture mapped port"),
        );
        Self {
            _container: container,
            endpoint,
        }
    }

    /// The `s3://` URL of `prefix` in the fixture bucket, in the form
    /// `compio_s3::S3Config::parse_url` reads.
    ///
    /// The fixture hands out plain data rather than `compio-s3` types:
    /// `compio-s3` dev-depends on this crate, so naming it here would put the
    /// crate under test on this crate's normal edge.
    #[must_use]
    pub fn url(&self, prefix: &str) -> String {
        format!(
            "s3://{BUCKET}/{prefix}?provider=generic&endpoint={}&region=us-east-1&style=path&dev_http=true&checksum=none",
            self.endpoint,
        )
    }

    /// The access key id the gateway accepts.
    #[must_use]
    pub fn access_key(&self) -> &'static str {
        ACCESS
    }

    /// The secret key the gateway accepts.
    #[must_use]
    pub fn secret_key(&self) -> &'static str {
        SECRET
    }
}