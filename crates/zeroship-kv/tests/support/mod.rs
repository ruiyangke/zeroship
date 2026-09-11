//! Docker fixtures shared by the storage and V8 integration tests.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::json;
use testcontainers::{
    core::{CmdWaitFor, ExecCommand, IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

const PORT: u16 = 6379;
const SLOT_RANGES: [(u16, u16); 3] = [(0, 5460), (5461, 10922), (10923, 16383)];

// Share fixtures while tests overlap, without retaining containers after the
// last test finishes. Each test binary gets an independent namespace and ports.
static FIXTURES: Mutex<Weak<Fixtures>> = Mutex::new(Weak::new());

pub fn fixtures() -> Arc<Fixtures> {
    let mut guard = FIXTURES
        .lock()
        .expect("KV fixture startup previously failed");
    if let Some(fixtures) = guard.upgrade() {
        return fixtures;
    }
    let fixtures = Arc::new(Fixtures::start());
    *guard = Arc::downgrade(&fixtures);
    fixtures
}

pub struct Fixtures {
    redis_config: zeroship_kv::RedisConfig,
    cluster_config: zeroship_kv::RedisConfig,
    _redis: Container<GenericImage>,
    _cluster: Vec<Container<GenericImage>>,
}

impl Fixtures {
    fn start() -> Self {
        let redis = start_redis();

        let cluster: Vec<_> = SLOT_RANGES
            .iter()
            .enumerate()
            .map(|(i, _)| {
                GenericImage::new("docker.dragonflydb.io/dragonflydb/dragonfly", "latest")
                    .with_exposed_port(PORT.tcp())
                    .with_wait_for(WaitFor::message_on_stderr("listening on"))
                    .with_cmd([
                        "--cluster_mode=yes".into(),
                        format!("--port={PORT}"),
                        format!("--cluster_node_id=node-{i}"),
                        "--logtostderr".into(),
                        "--proactor_threads=1".into(),
                        "--maxmemory=256mb".into(),
                    ])
                    .with_startup_timeout(Duration::from_secs(60))
                    .start()
                    .expect("KV tests require Docker to start the Dragonfly cluster")
            })
            .collect();

        let topology: Vec<_> = cluster
            .iter()
            .zip(SLOT_RANGES)
            .enumerate()
            .map(|(i, (node, (start, end)))| {
                let address = address(node);
                json!({
                    "slot_ranges": [{"start": start, "end": end}],
                    "master": {
                        "id": format!("node-{i}"),
                        "ip": address.ip().to_string(),
                        "port": address.port(),
                    },
                    "replicas": [],
                })
            })
            .collect();
        let topology = serde_json::to_string(&topology).unwrap();

        // Redis ships redis-cli. Execute its argv directly inside the container
        // to bootstrap peers over Docker's bridge; clients receive mapped host
        // endpoints from the topology, so no fixed host ports are needed.
        for node in &cluster {
            let mut result = redis
                .exec(
                    ExecCommand::new([
                        "timeout".into(),
                        "10".into(),
                        "redis-cli".into(),
                        "-h".into(),
                        node.get_bridge_ip_address()
                            .expect("Dragonfly bridge IP")
                            .to_string(),
                        "-p".into(),
                        PORT.to_string(),
                        "DFLYCLUSTER".into(),
                        "CONFIG".into(),
                        topology.clone(),
                    ])
                    .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
                )
                .expect("configure Dragonfly slots");
            let output = String::from_utf8(result.stdout_to_vec().unwrap()).unwrap();
            assert_eq!(output.trim(), "OK", "Dragonfly rejected its slot map");
        }

        let seeds = cluster
            .iter()
            .map(|node| address(node).to_string())
            .collect::<Vec<_>>();
        Self {
            redis_config: standalone(&redis),
            cluster_config: zeroship_kv::RedisConfig::new(zeroship_kv::Topology::Cluster { seeds }),
            _redis: redis,
            _cluster: cluster,
        }
    }

    pub fn redis_config(&self) -> zeroship_kv::RedisConfig {
        self.redis_config.clone()
    }

    pub fn cluster_config(&self) -> zeroship_kv::RedisConfig {
        self.cluster_config.clone()
    }
}

pub fn start_redis() -> Container<GenericImage> {
    GenericImage::new("redis", "7")
        .with_exposed_port(PORT.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_startup_timeout(Duration::from_secs(60))
        .start()
        .expect("KV tests require Docker to start Redis")
}

fn address(container: &Container<GenericImage>) -> SocketAddr {
    // The compio Redis driver requires a numeric IP. Resolve Docker's host
    // here, selecting the address family used by get_host_port_ipv4.
    let host = container.get_host().expect("Docker host").to_string();
    let port = container
        .get_host_port_ipv4(PORT)
        .expect("mapped Redis port");
    (host.as_str(), port)
        .to_socket_addrs()
        .expect("resolve Docker host")
        .find(SocketAddr::is_ipv4)
        .expect("Docker host needs an IPv4 address for the mapped port")
}

pub fn standalone(container: &Container<GenericImage>) -> zeroship_kv::RedisConfig {
    zeroship_kv::RedisConfig::new(zeroship_kv::Topology::Standalone {
        endpoint: address(container).to_string(),
    })
}
