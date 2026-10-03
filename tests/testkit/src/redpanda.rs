//! The Redpanda broker every stream test process of a worktree shares.
//!
//! [`broker()`] joins the one broker a worktree boots through [`crate::shared`]:
//! the first process elects itself, the image is built, the container starts,
//! and every other process joins the ready broker. A process holds its lease for
//! as long as it runs; the container's watchdog removes the broker once no
//! process has held it for the idle grace. One broker per run is what keeps the
//! suite under the host's global `fs.aio-max-nr`: Seastar refuses to start when
//! several brokers boot at once.

use std::sync::OnceLock;

use crate::shared::{self, HostPort, Port, Readiness, Scope, Spec};

mod image;

use image::{build, reference as image_ref};

/// The port Redpanda's Kafka API listens on in the image.
const KAFKA_PORT: u16 = 19092;

/// The watchdog baked into the broker image.
const WATCHDOG: &str = "/usr/local/bin/zeroship-watchdog";

/// The line Redpanda logs once its Kafka API is up.
const READY_MARKER: &str = "Successfully started Redpanda!";

/// A shared Redpanda broker, leased by the process that holds it.
pub struct Broker {
    lease: shared::Lease,
}

impl Broker {
    /// The `host:port` a Kafka client connects to.
    #[must_use]
    pub fn brokers(&self) -> String {
        format!("127.0.0.1:{}", self.lease.port)
    }

    /// The Docker id of the shared broker.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
    }
}

/// Join the worktree's shared broker, booting it if this process is elected.
///
/// # Panics
/// When the broker image cannot be built, or the broker cannot be booted or
/// joined.
#[must_use]
pub fn broker() -> &'static Broker {
    static BROKER: OnceLock<Broker> = OnceLock::new();
    BROKER.get_or_init(|| {
        let spec = spec().unwrap_or_else(|error| {
            panic!("the shared Redpanda broker recipe could not be built: {error}")
        });
        let lease = shared::join(&Scope::worktree("redpanda"), &spec, |_| Ok(()))
            .unwrap_or_else(|error| panic!("the shared Redpanda broker could not start: {error}"));
        Broker { lease }
    })
}

/// The recipe the shared broker runs under, its identity keyed to every input
/// that changes what a ready broker holds.
///
/// # Errors
/// When the image cannot be built.
pub fn spec() -> Result<Spec, String> {
    let _ = build()
        .map_err(|error| format!("could not build the shared Redpanda image: {error}"))?;
    let image = image_ref();
    let args = vec![
        "rpk".to_owned(),
        "redpanda".to_owned(),
        "start".to_owned(),
        "--overprovisioned".to_owned(),
        "--smp".to_owned(),
        "1".to_owned(),
        "--memory".to_owned(),
        "512M".to_owned(),
        "--reserve-memory".to_owned(),
        "0M".to_owned(),
        "--node-id".to_owned(),
        "0".to_owned(),
        "--check=false".to_owned(),
        "--kafka-addr".to_owned(),
        format!("external://0.0.0.0:{KAFKA_PORT}"),
        "--advertise-kafka-addr".to_owned(),
        format!("external://127.0.0.1:{}", shared::HOST_PORT_TOKEN),
        "--set".to_owned(),
        "redpanda.auto_create_topics_enabled=true".to_owned(),
    ];
    let environment = Vec::new();
    let ready = readiness();
    let inputs = inputs(&image, &environment, &args, &ready);
    Ok(Spec {
        inputs,
        image,
        environment,
        ports: vec![Port {
            container: KAFKA_PORT,
            host: HostPort::Reserved,
        }],
        watchdog: WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Watchdog,
        args,
        ready,
    })
}

/// How to tell the broker is ready and still answers.
fn readiness() -> Readiness {
    let health = vec![
        "rpk".to_owned(),
        "cluster".to_owned(),
        "health".to_owned(),
    ];
    Readiness {
        log_marker: READY_MARKER.to_owned(),
        probe: health.clone(),
        answer: health,
    }
}

/// The 12-hex identity of a broker built from the image, environment, server
/// command and readiness probe.
fn inputs(
    image: &str,
    environment: &[(String, String)],
    args: &[String],
    ready: &Readiness,
) -> String {
    let mut parts: Vec<Vec<u8>> = vec![image.as_bytes().to_vec(), KAFKA_PORT.to_string().into_bytes()];
    for argument in args {
        parts.push(argument.as_bytes().to_vec());
    }
    for (key, value) in environment {
        parts.push(format!("{key}={value}").into_bytes());
    }
    parts.push(ready.log_marker.as_bytes().to_vec());
    for argument in ready.probe.iter().chain(&ready.answer) {
        parts.push(argument.as_bytes().to_vec());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    shared::digest(&refs)
}
