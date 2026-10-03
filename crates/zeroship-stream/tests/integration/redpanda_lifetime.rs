//! The private Redpanda broker's own lifetime.
//!
//! These cases measure the reaper-owned broker's lifetime, so they start a
//! private broker rather than joining the worktree's shared one.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{GenericImage, ImageExt};

use zeroship_testkit::lifetime;
use zeroship_testkit::{start_owned, DockerCli, OwnedContainer, Ownership};

struct Redpanda {
    owned: OwnedContainer,
    brokers: String,
}

impl Redpanda {
    fn start() -> Self {
        let port = available_port();
        let advertised = format!("external://127.0.0.1:{port}");
        let request = GenericImage::new("docker.redpanda.com/redpandadata/redpanda", "v26.2.2")
            .with_wait_for(WaitFor::message_on_stderr("Successfully started Redpanda!"))
            .with_mapped_port(port, 19092.tcp())
            .with_cmd([
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
                "external://0.0.0.0:19092".to_owned(),
                "--advertise-kafka-addr".to_owned(),
                advertised,
                "--set".to_owned(),
                "redpanda.auto_create_topics_enabled=true".to_owned(),
            ])
            .with_startup_timeout(Duration::from_secs(120));
        let owned = start_owned(&DockerCli::system(), &Ownership::mint(), request)
            .unwrap_or_else(|error| panic!("stream tests require Docker and Redpanda: {error}"));
        Self {
            owned,
            brokers: format!("127.0.0.1:{port}"),
        }
    }
}

fn available_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("bind an available Redpanda port")
        .local_addr()
        .expect("Redpanda listener address")
        .port()
}

static REDPANDA: OnceLock<Redpanda> = OnceLock::new();

/// The child test the broker's lifetime measurements run, by its full path in this
/// binary.
///
/// `module_path!()` prefixes the crate name; libtest names a case by its module
/// path without that prefix, so the first segment is dropped.
fn child_test() -> String {
    let module = module_path!();
    let module = module.split_once("::").map_or(module, |(_, path)| path);
    format!("{module}::the_redpanda_broker_reports_its_container")
}

#[test]
fn the_redpanda_broker_reports_its_container() {
    lifetime::report_owner();
    let broker = REDPANDA.get_or_init(Redpanda::start);
    assert!(
        broker.brokers.starts_with("127.0.0.1:"),
        "{}",
        broker.brokers
    );
    lifetime::report_container(broker.owned.container().id());
}

#[test]
fn the_redpanda_broker_is_removed_when_its_process_ends() {
    lifetime::assert_removed_after_the_child_exits(&child_test());
}

#[test]
fn the_redpanda_broker_is_removed_when_its_process_is_killed_while_starting() {
    lifetime::assert_removed_after_a_kill_during_startup(&child_test());
}
