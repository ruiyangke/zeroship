//! Settings that decide whether a workflow host can run at all.

use super::*;

fn config(manager_url: &str) -> WorkflowHostConfig {
    WorkflowHostConfig {
        manager_url: manager_url.to_owned(),
        capacity: 8,
        slots: 2,
        plaintext_peers: PlaintextPeers::default(),
    }
}

// The host signs every manager exchange with this instance's enrolled key, so
// the origin it sends them to is part of the credential's trust boundary: a
// plaintext manager anywhere but this machine would hand those assertions to
// the network.
#[test]
fn a_manager_origin_is_https_or_plain_http_to_a_literal_loopback_address_by_default() {
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

// The one-variable control at the worker's own configuration surface: ONE
// settings value naming one origin, and two manager URLs judged by it.
//
// The list also reaches the deployment-hold client this host builds per
// assignment, which `client_options` is what ties together; that client is
// constructed against Control's origin rather than the manager's, so it is not
// reachable from this pure check.
#[test]
fn a_named_plaintext_peer_is_a_usable_manager_origin_and_an_unnamed_one_is_not() {
    let named: PlaintextPeers = std::iter::once("http://workflow:9093")
        .map(|origin| origin.parse().expect("a valid plaintext peer"))
        .collect();

    let mut admitted = config("http://workflow:9093");
    admitted.plaintext_peers = named.clone();
    admitted
        .validate()
        .expect("a manager origin this worker named is usable");

    let mut refused = config("http://control:9090");
    refused.plaintext_peers = named;
    let error = refused
        .validate()
        .expect_err("an origin this worker did not name stays refused");
    assert!(
        error.contains("worker.workflow_manager_url"),
        "the refusal names the setting: {error}"
    );

    // The control on the control: the SAME admitted URL under the default
    // settings is refused, so the admission above is the list's doing.
    assert!(config("http://workflow:9093").validate().is_err());
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

/// The journal's schema is a choice the host carries, not a fact about the app.
///
/// This is the assertion that separates a change from a rename: a parameter
/// every caller filled with the same value would pass a test that only checked
/// the parameter exists. Here ONE configured schema answers for TWO different
/// apps, and the inequality of the apps is asserted beside it so "both got the
/// same schema" cannot pass over two apps that were secretly the same.
///
/// What this does NOT catch: it binds the decision, not the wiring. A future
/// edit that made `resolve` derive the schema itself again would leave this
/// green. The compiler is what ties them together today - `resolve` has no
/// other way to reach a schema.
#[test]
fn a_service_journal_answers_one_schema_for_every_app() {
    let one = AppId::mint();
    let two = AppId::mint();
    assert_ne!(one.as_str(), two.as_str(), "the two apps must differ");

    let shared = SchemaName::new("workflow_manager").expect("a legal schema name");
    let service = JournalLocation::Service(shared.clone());

    assert_eq!(
        service.schema(&one).expect("service journal schema").as_str(),
        shared.as_str()
    );
    assert_eq!(
        service.schema(&two).expect("service journal schema").as_str(),
        shared.as_str()
    );
}

/// The creator arm keeps today's behaviour, and keeps it DIFFERENT from the
/// service arm: each app journals in its own schema. Without this the two arms
/// could answer identically and the choice above would be decorative.
#[test]
fn a_creator_journal_answers_each_app_its_own_schema() {
    let one = AppId::mint();
    let two = AppId::mint();
    let creator = JournalLocation::CreatorSchema;

    assert_eq!(
        creator.schema(&one).expect("creator journal schema").as_str(),
        one.as_str()
    );
    assert_ne!(
        creator.schema(&one).expect("creator journal schema").as_str(),
        creator.schema(&two).expect("creator journal schema").as_str()
    );
}
