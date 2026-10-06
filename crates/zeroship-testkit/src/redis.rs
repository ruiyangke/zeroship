//! The shared Redis and Dragonfly servers for the driver, KV and binding
//! suites.
//!
//! [`redis()`] and [`cluster()`] each join one server a worktree boots through
//! [`zeroship_shared_server`]: the first process elects itself, the image is built, the
//! container starts, and every other process of the run joins the ready
//! server. A process holds its lease for as long as it runs; each container's
//! watchdog removes its server once no process has held it for the idle
//! grace. The two servers have independent lazy accessors - not one combined
//! fixture - so a test that only needs the standalone server never boots, and
//! never fails on, the cluster.
//!
//! This module hands out only URLs, ports and the minted [`case_prefix`]: it
//! carries no dependency on `compio-redis`, because `compio-redis` itself is
//! a consumer of this testkit (`cargo xtask test repository`'s
//! `no_dev_only_package_depends_on_a_crate_whose_tests_use_it` enforces this
//! shape at the manifest level, not just at the type level - a `RedisConfig`
//! built here would pull the forbidden edge straight back). A caller that
//! wants a typed `RedisConfig`/`Topology` builds it in its own test support
//! from the strings here, as `zeroship-kv` and `zeroship-kv-v8`'s
//! `tests/support` do.
//!
//! Both servers are shared by every case of a run, so no case may flush the
//! keyspace or assume it starts empty: [`case_prefix`] mints a value no other
//! case holds, for a case to scope its own keys (or app id) under, and cluster
//! callers wrap it in `{}` to keep one case's keys in one slot. A case whose
//! subject is the server's own configuration - cluster topology, replication,
//! TLS, a port it must stop - starts a private server of its own instead with
//! [`start_redis`] or raw `testcontainers`, because that subject is exactly
//! what every other case on the shared servers must not see change.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use testcontainers::{Container, GenericImage, ImageExt};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;

use zeroship_shared_server::{self as shared, Boot, Entrypoint, HostPort, Port, Readiness, Scope, Spec};

mod image;

pub use image::reference as cluster_image;

/// The port the standalone Redis server listens on.
const STANDALONE_PORT: u16 = 6379;

/// The watchdog baked into both the standalone and cluster images.
const WATCHDOG: &str = zeroship_shared_server::image::WATCHDOG;

/// The wrapper the cluster image runs as the watchdog's single tracked child.
const CLUSTER_WRAPPER: &str = "/usr/local/bin/zeroship-dragonfly-cluster";

/// The container-internal port the cluster's first node listens on; the rest
/// follow at consecutive ports, one per node.
const CLUSTER_BASE_PORT: u16 = 7000;

/// The number of nodes in the shared Dragonfly cluster.
const CLUSTER_NODES: usize = 3;

/// The hash slot range each cluster node owns, in node order.
const SLOT_RANGES: [(u16, u16); CLUSTER_NODES] = [(0, 5460), (5461, 10922), (10923, 16383)];

/// The worktree's shared standalone Redis server.
#[derive(Debug)]
pub struct Redis {
    lease: shared::Lease,
}

impl Redis {
    /// The `redis://host:port` URL a client connects with.
    #[must_use]
    pub fn url(&self) -> String {
        format!("redis://{}", self.endpoint())
    }

    /// The bare `host:port` a `Topology::Standalone` names.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.lease.port)
    }

    /// The mapped host port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.lease.port
    }

    /// The Docker id of the shared standalone server.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
    }
}

/// Join the worktree's shared standalone Redis server, booting it if this
/// process is elected.
///
/// # Panics
/// When the image cannot be built, or the server cannot be booted or joined.
#[must_use]
pub fn redis() -> &'static Redis {
    static REDIS: OnceLock<Redis> = OnceLock::new();
    REDIS.get_or_init(|| {
        let spec = standalone_spec()
            .unwrap_or_else(|error| panic!("the shared Redis server recipe could not be built: {error}"));
        let lease = shared::join(&Scope::worktree("redis"), &spec, |_| Ok(()))
            .unwrap_or_else(|error| panic!("the shared Redis server could not start: {error}"));
        Redis { lease }
    })
}

/// The worktree's shared three-node Dragonfly cluster.
#[derive(Debug)]
pub struct Cluster {
    lease: shared::Lease,
    ports: [u16; CLUSTER_NODES],
}

impl Cluster {
    /// The `redis://host:port` seed URL of every node, in node order.
    #[must_use]
    pub fn urls(&self) -> Vec<String> {
        self.endpoints().into_iter().map(|endpoint| format!("redis://{endpoint}")).collect()
    }

    /// The bare `host:port` seeds a `Topology::Cluster` names, in node order.
    #[must_use]
    pub fn endpoints(&self) -> Vec<String> {
        self.ports.iter().map(|port| format!("127.0.0.1:{port}")).collect()
    }

    /// The Docker id of the cluster's one container.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
    }
}

/// Join the worktree's shared Dragonfly cluster, booting and configuring its
/// slot map if this process is elected.
///
/// # Panics
/// When the image cannot be built, or the cluster cannot be booted or joined.
#[must_use]
pub fn cluster() -> &'static Cluster {
    static CLUSTER: OnceLock<Cluster> = OnceLock::new();
    CLUSTER.get_or_init(|| {
        let spec = cluster_spec().unwrap_or_else(|error| {
            panic!("the shared Dragonfly cluster recipe could not be built: {error}")
        });
        let lease = shared::join(&Scope::worktree("redis-cluster"), &spec, boot_cluster)
            .unwrap_or_else(|error| panic!("the shared Dragonfly cluster could not start: {error}"));
        let ports = cluster_host_ports(&lease.container_id).unwrap_or_else(|error| {
            panic!("could not read the shared Dragonfly cluster's mapped ports: {error}")
        });
        Cluster { lease, ports }
    })
}

/// A key, app id or cluster hash-tag prefix no other case on the shared
/// servers holds.
///
/// The standalone server and the cluster are shared by every test process of
/// a run, so a literal key or app id could collide with a concurrent case's; a
/// case built on [`redis()`] or [`cluster()`] scopes every key it touches
/// under a prefix this mints.
#[must_use]
pub fn case_prefix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!(
        "case{}n{nanos}c{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The recipe the shared standalone Redis server runs under, its identity
/// keyed to every input that changes what a ready server holds.
///
/// # Errors
/// When the image cannot be built.
pub fn standalone_spec() -> Result<Spec, String> {
    let image = zeroship_shared_server::image::with_watchdog(&crate::images::REDIS_7.reference())?;
    let args = vec!["docker-entrypoint.sh".to_owned(), "redis-server".to_owned()];
    let ready = standalone_readiness();
    let inputs = standalone_inputs(&image, &args, &ready);
    Ok(Spec {
        inputs,
        image,
        environment: Vec::new(),
        ports: vec![Port {
            container: STANDALONE_PORT,
            host: HostPort::Assigned,
        }],
        watchdog: WATCHDOG.to_owned(),
        entrypoint: Entrypoint::Image,
        args,
        ready,
    })
}

fn standalone_readiness() -> Readiness {
    let ping = vec!["redis-cli".to_owned(), "-p".to_owned(), STANDALONE_PORT.to_string(), "PING".to_owned()];
    Readiness {
        log_marker: "Ready to accept connections".to_owned(),
        probe: ping.clone(),
        answer: ping,
    }
}

/// The 12-hex identity of a standalone server built from the image, server
/// command and readiness probe.
fn standalone_inputs(image: &str, args: &[String], ready: &Readiness) -> String {
    let mut parts: Vec<Vec<u8>> = vec![image.as_bytes().to_vec(), STANDALONE_PORT.to_string().into_bytes()];
    for argument in args {
        parts.push(argument.as_bytes().to_vec());
    }
    parts.push(ready.log_marker.as_bytes().to_vec());
    for argument in ready.probe.iter().chain(&ready.answer) {
        parts.push(argument.as_bytes().to_vec());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    shared::digest(&refs)
}

/// The recipe the shared Dragonfly cluster's one container runs under, its
/// identity keyed to every input that changes what a ready cluster holds.
///
/// # Errors
/// When the cluster image cannot be built.
pub fn cluster_spec() -> Result<Spec, String> {
    let image = cluster_image()
        .map_err(|error| format!("could not build the shared Dragonfly cluster image: {error}"))?;
    let args = vec![
        CLUSTER_WRAPPER.to_owned(),
        CLUSTER_BASE_PORT.to_string(),
        CLUSTER_NODES.to_string(),
    ];
    let ready = cluster_readiness();
    let ports = (0..CLUSTER_NODES)
        .map(|i| Port {
            container: CLUSTER_BASE_PORT + i as u16,
            host: HostPort::Assigned,
        })
        .collect();
    let inputs = cluster_inputs(&image, &args, &ready);
    Ok(Spec {
        inputs,
        image,
        environment: Vec::new(),
        ports,
        watchdog: WATCHDOG.to_owned(),
        entrypoint: Entrypoint::Watchdog,
        args,
        ready,
    })
}

/// How to tell every node of the cluster is ready and still answers: every
/// node's `PING` must succeed, because a node still initialising cannot take
/// its slot assignment, and a node that has crashed must not read as ready.
fn cluster_readiness() -> Readiness {
    let script = cluster_ping_script();
    let probe = vec!["sh".to_owned(), "-c".to_owned(), script];
    Readiness {
        log_marker: "listening on".to_owned(),
        probe: probe.clone(),
        answer: probe,
    }
}

fn cluster_ping_script() -> String {
    let mut script = String::from("set -e; ");
    for i in 0..CLUSTER_NODES {
        let port = CLUSTER_BASE_PORT + i as u16;
        script.push_str(&format!("redis-cli -p {port} PING | grep -q PONG; "));
    }
    script
}

fn cluster_inputs(image: &str, args: &[String], ready: &Readiness) -> String {
    let mut parts: Vec<Vec<u8>> = vec![image.as_bytes().to_vec()];
    for argument in args {
        parts.push(argument.as_bytes().to_vec());
    }
    parts.push(ready.log_marker.as_bytes().to_vec());
    for argument in ready.probe.iter().chain(&ready.answer) {
        parts.push(argument.as_bytes().to_vec());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    shared::digest(&refs)
}

/// Assign the cluster's slot ranges to its nodes, once, the first time the
/// container boots.
///
/// Every node receives the full topology - the slot range, id and host-mapped
/// `ip:port` of every node, including itself - because that is what lets a
/// client's redirect from one node name the others by an address the client,
/// running on the host rather than inside the container, can reach.
fn boot_cluster(boot: &Boot) -> Result<(), String> {
    let ports = cluster_host_ports(&boot.container_id)?;
    let topology: Vec<_> = SLOT_RANGES
        .iter()
        .enumerate()
        .map(|(i, (start, end))| {
            json!({
                "slot_ranges": [{"start": start, "end": end}],
                "master": {
                    "id": format!("node-{i}"),
                    "ip": "127.0.0.1",
                    "port": ports[i],
                },
                "replicas": [],
            })
        })
        .collect();
    let topology = serde_json::to_string(&topology).map_err(|error| error.to_string())?;

    for i in 0..CLUSTER_NODES {
        let port = (CLUSTER_BASE_PORT + i as u16).to_string();
        let output = boot.exec(
            "redis-cli",
            &["-p", port.as_str(), "DFLYCLUSTER", "CONFIG", topology.as_str()],
        )?;
        if !output.success() {
            return Err(format!(
                "configuring Dragonfly node {i}'s slots failed (exit {:?}): {}",
                output.exit_code,
                output.stderr_text()
            ));
        }
        let reply = String::from_utf8_lossy(&output.stdout);
        if reply.trim() != "OK" {
            return Err(format!("Dragonfly node {i} rejected its slot map: {reply}"));
        }
    }
    Ok(())
}

/// The host-mapped port of every node in the cluster container, in node order.
///
/// A port mapping is fixed once the container is created, so this can run
/// from the boot closure or from any later process that joins the lease.
///
/// # Errors
/// When the daemon cannot be asked for one of the mappings.
fn cluster_host_ports(container_id: &str) -> Result<[u16; CLUSTER_NODES], String> {
    let mut ports = [0u16; CLUSTER_NODES];
    for (i, port) in ports.iter_mut().enumerate() {
        *port = shared::mapped_port(container_id, CLUSTER_BASE_PORT + i as u16)?;
    }
    Ok(ports)
}

// --- private, single-use servers --------------------------------------------
//
// For a case whose subject is the server itself - one it must stop, or whose
// configuration it mutates - rather than data on it.

/// Start a standalone Redis container this case owns and must stop or drop
/// itself; not part of the shared fixture.
///
/// # Panics
/// When Docker cannot start the container.
#[must_use]
pub fn start_redis() -> Container<GenericImage> {
    crate::images::REDIS_7.generic()
        .with_exposed_port(STANDALONE_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_startup_timeout(Duration::from_secs(60))
        .start()
        .expect("Redis tests require Docker to start Redis")
}

/// The bare `host:port` of a privately-started standalone Redis container, for
/// a caller to build its own typed configuration from.
#[must_use]
pub fn endpoint(container: &Container<GenericImage>) -> String {
    address(container).to_string()
}

fn address(container: &Container<GenericImage>) -> std::net::SocketAddr {
    use std::net::ToSocketAddrs;
    // Match the address family of the mapped Docker port.
    let host = container.get_host().expect("Docker host").to_string();
    let port = container
        .get_host_port_ipv4(STANDALONE_PORT)
        .expect("mapped Redis port");
    (host.as_str(), port)
        .to_socket_addrs()
        .expect("resolve Docker host")
        .find(std::net::SocketAddr::is_ipv4)
        .expect("Docker host needs an IPv4 address for the mapped port")
}

#[cfg(test)]
mod tests {
    use super::case_prefix;

    #[test]
    fn each_call_mints_a_distinct_ascii_prefix() {
        let first = case_prefix();
        let second = case_prefix();
        assert_ne!(first, second);
        for prefix in [&first, &second] {
            assert!(!prefix.is_empty());
            assert!(
                prefix.bytes().all(|b| b.is_ascii_alphanumeric()),
                "a prefix must be plain ASCII alphanumerics so it is valid both as a \
                 standalone key segment and inside a cluster hash tag: {prefix:?}"
            );
        }
    }
}
