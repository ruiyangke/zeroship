//! An owned S3-compatible server with Docker-assigned ports.
//!
//! The server is the Versity S3 Gateway over its POSIX backend: a real S3
//! implementation that verifies `SigV4` (an unsigned or wrongly-signed request is
//! refused), assembles genuine multipart uploads and answers ranged GETs, so a
//! signing or multipart regression fails here instead of passing against a mock
//! that accepts anything.
//!
//! The container is started through the shared reaper ([`docker`]), so a process
//! killed before this value is dropped still has its gateway removed.

#[path = "docker.rs"]
mod docker;

use std::time::Duration;

use testcontainers::{
    core::{IntoContainerPort, Mount, WaitFor},
    GenericImage, ImageExt,
};

use docker::{start_owned, DockerCli, OwnedContainer, Ownership};

const IMAGE: &str = "ghcr.io/versity/versitygw";
/// Pinned: a floating tag is how a fixture silently changes servers under a
/// green suite, or stops resolving when a tag is withdrawn.
const TAG: &str = "v1.3.0";
const ACCESS: &str = "zeroship-fixture";
const SECRET: &str = "zeroship-fixture-secret";
const BUCKET: &str = "storage-fixture";
const PORT: u16 = 7070;
const DATA_CAPACITY: i64 = 1024 * 1024 * 1024;

pub struct S3Server {
    _container: OwnedContainer,
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
        let container = start_owned(
            &DockerCli::system(),
            &Ownership::mint(),
            GenericImage::new(IMAGE, TAG)
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
                .with_startup_timeout(Duration::from_secs(90)),
        )
        .expect("S3 tests require Docker");
        let endpoint = format!(
            "http://{}:{}",
            container.container().get_host().expect("S3 fixture host"),
            container
                .container()
                .get_host_port_ipv4(PORT.tcp())
                .expect("S3 fixture mapped port"),
        );
        Self {
            _container: container,
            endpoint,
        }
    }

    pub fn config(&self, prefix: &str) -> compio_s3::S3Config {
        compio_s3::S3Config::parse_url(&format!(
            "s3://{BUCKET}/{prefix}?provider=generic&endpoint={}&region=us-east-1&style=path&dev_http=true&checksum=none",
            self.endpoint,
        )).expect("S3 fixture configuration")
    }

    pub fn credentials(&self) -> compio_s3::S3Credentials {
        compio_s3::S3Credentials::new(ACCESS, SECRET, None)
    }
}
