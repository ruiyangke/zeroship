//! The Redpanda broker's own lifetime.
//!
//! The stream suites join the worktree's shared broker, whose removal is the
//! watchdog's once the run's last lease is gone. These cases measure that
//! against real child processes: a child joins the broker recipe at a throwaway
//! scope and exits, or is `SIGKILL`ed while the broker is still starting, and
//! the broker must be removed either way; see `zeroship_testkit::lifetime`.

use zeroship_testkit::lifetime;
use zeroship_testkit::redpanda::Broker;

/// The child test the broker's lifetime measurements run, by its full path in
/// this binary.
///
/// `module_path!()` prefixes the crate name; libtest names a case by its module
/// path without that prefix, so the first segment is dropped.
fn child_test() -> String {
    let module = module_path!();
    let module = module.split_once("::").map_or(module, |(_, path)| path);
    format!("{module}::child_joins_the_broker_at_a_throwaway_scope")
}

#[test]
#[ignore = "spawned by a lifetime measurement as a child process, with its scope on stdin"]
fn child_joins_the_broker_at_a_throwaway_scope() {
    let broker = Broker::join(&lifetime::child_scope()).expect("join the broker recipe");
    assert!(
        broker.brokers().starts_with("127.0.0.1:"),
        "{}",
        broker.brokers()
    );
    lifetime::report_container(broker.container_id());
}

#[test]
fn the_redpanda_broker_is_removed_when_its_process_ends() {
    lifetime::assert_removed_after_the_child_exits(&child_test());
}

#[test]
fn the_redpanda_broker_is_removed_when_its_process_is_killed_while_starting() {
    lifetime::assert_removed_after_a_kill_during_startup(&child_test());
}
