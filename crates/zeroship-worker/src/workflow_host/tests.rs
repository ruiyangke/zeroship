//! Settings that decide whether a workflow host can run at all.

use super::*;

fn config(manager_url: &str) -> WorkflowHostConfig {
    WorkflowHostConfig {
        manager_url: manager_url.to_owned(),
        capacity: 8,
        slots: 2,
    }
}

// The host signs every manager exchange with this instance's enrolled key, so
// the origin it sends them to is part of the credential's trust boundary: a
// plaintext manager anywhere but this machine would hand those assertions to
// the network.
#[test]
fn a_manager_origin_is_https_or_plain_http_only_to_a_literal_loopback_address() {
    for url in [
        "https://workflow.example",
        "https://workflow.example:9443",
        "http://127.0.0.1:9095",
        "http://[::1]:9095",
    ] {
        config(url)
            .validate()
            .unwrap_or_else(|error| panic!("{url} is a usable manager origin: {error}"));
    }
    for url in [
        "",
        "workflow.example",
        "http://workflow.example",
        // "localhost" is a name, not a literal address, and resolves wherever
        // the host's resolver says.
        "http://localhost:9095",
        "https://user:pass@workflow.example",
        "https://workflow.example/manager",
        "https://workflow.example/?tenant=a",
        "ftp://workflow.example",
    ] {
        let error = config(url)
            .validate()
            .expect_err("an unusable manager origin is refused");
        assert!(error.contains("worker.workflow_manager_url"), "{url}: {error}");
    }
}

// Capacity is advertised to the manager as the placements this worker will
// accept, and slots bound concurrent execution. Neither has a meaningful zero,
// and capacity crosses the wire as a u32.
#[test]
fn capacity_and_slots_must_be_positive_and_capacity_must_cross_the_wire() {
    let mut zero_capacity = config("https://workflow.example");
    zero_capacity.capacity = 0;
    assert!(zero_capacity
        .validate()
        .expect_err("zero capacity is refused")
        .contains("worker.workflow_capacity"));

    let mut unrepresentable = config("https://workflow.example");
    unrepresentable.capacity = usize::MAX;
    assert!(unrepresentable
        .validate()
        .expect_err("capacity beyond the wire type is refused")
        .contains("worker.workflow_capacity"));

    let mut zero_slots = config("https://workflow.example");
    zero_slots.slots = 0;
    assert!(zero_slots
        .validate()
        .expect_err("zero slots is refused")
        .contains("worker.workflow_slots"));

    // The control: the same settings with positive bounds are accepted.
    config("https://workflow.example")
        .validate()
        .expect("positive bounds run a host");
}

// The manager counts placements; the consumer counts executing jobs. Neither
// bound may silently become the other, and the assignment registry must never
// admit more apps than the consumer can hold scopes for.
#[test]
fn the_host_options_carry_capacity_to_placements_and_slots_to_execution() {
    let mut settings = config("https://workflow.example");
    settings.capacity = 12;
    settings.slots = 3;
    let options = settings.host_options();
    assert_eq!(options.assignments.max_scopes, 12);
    assert_eq!(options.consumer.max_scopes, 12);
    assert_eq!(options.consumer.slots, 3);
    assert!(!options.assignments.operation_timeout.is_zero());
    assert!(!options.registration_interval.is_zero());
    assert!(!options.assignment_interval.is_zero());
    assert!(!options.policy_interval.is_zero());
}

// With no host running - the default deployment - the registry every request
// isolate resolves through is empty, so `env.workflows` refuses rather than
// reaching any other backend.
#[test]
fn an_app_with_no_running_host_is_never_ready() {
    let apps = ReadyApps::default();
    assert!(!apps.is_ready(&AppId::mint()));
}
