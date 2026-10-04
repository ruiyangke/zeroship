//! Settings that decide whether a workflow host can run at all.

use super::*;

fn config(manager_url: &str) -> WorkflowHostConfig {
    WorkflowHostConfig {
        manager_url: manager_url.to_owned(),
        prepared_apps: 8,
        slots: 2,
        plaintext_peers: PlaintextPeers::default(),
        shutdown_timeout: MIN_SHUTDOWN_TIMEOUT,
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

// Slots bound concurrent execution, and every executing delivery holds its
// prepared app, so a prepared-app bound below the slot count could only evict
// an app a running execution still holds. Neither bound has a meaningful zero.
#[test]
fn prepared_apps_must_cover_the_slots_and_slots_must_be_positive() {
    let mut fewer = config("https://workflow.example");
    fewer.slots = 3;
    fewer.prepared_apps = 2;
    let error = fewer
        .validate()
        .expect_err("fewer prepared apps than slots is refused");
    assert!(
        error.contains("worker.workflow_prepared_apps"),
        "the refusal names the setting: {error}"
    );

    let mut zero_slots = config("https://workflow.example");
    zero_slots.slots = 0;
    assert!(zero_slots
        .validate()
        .expect_err("zero slots is refused")
        .contains("worker.workflow_slots"));

    // The control: one prepared app per slot is exactly enough.
    let mut equal = config("https://workflow.example");
    equal.slots = 3;
    equal.prepared_apps = 3;
    equal
        .validate()
        .expect("as many prepared apps as slots runs a host");
}

// The prepared-app cache counts apps; the consumer counts executing jobs.
// Neither bound may silently become the other.
#[test]
fn the_host_options_carry_prepared_apps_to_the_cache_and_slots_to_execution() {
    let mut settings = config("https://workflow.example");
    settings.prepared_apps = 12;
    settings.slots = 3;
    let options = settings.host_options();
    assert_eq!(options.prepared.capacity, 12);
    assert_eq!(options.consumer.slots, 3);
    assert!(!options.prepared.operation_timeout.is_zero());
    assert!(!options.consumer.delivery.operation_timeout.is_zero());
}

// The host's running deliveries get the drain minus the bound that releases
// what they could not finish, and at the shortest drain that grace still holds
// a delivery claimed just before the stop through its app's preparation, its
// execution and its settlement, each at the bound the host itself runs it under.
// It also holds the claim a stop can find pending, which runs beside it: the
// claim's wait and the bound after it, then the give-back of what it delivers.
#[test]
fn the_running_deliveries_grace_is_the_drain_less_its_release() {
    let mut settings = config("https://workflow.example");
    let longer = MIN_SHUTDOWN_TIMEOUT + Duration::from_secs(25);
    settings.shutdown_timeout = longer;
    assert_eq!(
        settings.host_options().consumer.drain,
        longer.checked_sub(OPERATION_TIMEOUT).unwrap()
    );

    settings.shutdown_timeout = MIN_SHUTDOWN_TIMEOUT;
    let options = settings.host_options();
    let grace = options.consumer.drain;
    assert_eq!(grace, MIN_SHUTDOWN_TIMEOUT.checked_sub(OPERATION_TIMEOUT).unwrap());
    let delivery = options.consumer.delivery;
    let one_delivery =
        options.prepared.operation_timeout + delivery.execution_timeout + delivery.operation_timeout;
    assert!(
        grace >= one_delivery,
        "a grace of {grace:?} cuts off a delivery that needs {one_delivery:?}"
    );
    // The consumer states the operation bound as its claim's wait, cuts the
    // claim one operation bound after it, and bounds each give-back by one.
    let claim_tail = delivery.operation_timeout * 3;
    assert!(
        grace >= claim_tail,
        "a grace of {grace:?} ends before a claim pending at the stop, which needs \
         {claim_tail:?}"
    );
}

mod drain;
mod residency;

// A stopping worker lets a delivery it claimed just before the stop run to the
// execution ceiling and then settles it, so a shorter drain would cut that
// delivery off and leave it leased until its lease lapses.
#[test]
fn a_shutdown_timeout_must_cover_one_execution_at_the_ceiling_and_its_settlement() {
    let minimum = MIN_SHUTDOWN_TIMEOUT.as_secs();
    assert!(
        MIN_SHUTDOWN_TIMEOUT > EXECUTION_CEILING,
        "the premise: the requirement includes the settlement after the execution"
    );
    for short in [0, minimum - 1] {
        let error = validate_shutdown_timeout(short)
            .expect_err("a drain shorter than one execution and its settlement is refused");
        assert!(
            error.contains("worker.shutdown_timeout"),
            "the refusal names the setting: {error}"
        );
    }
    // The control: exactly the requirement, and anything above it, is accepted.
    assert_eq!(validate_shutdown_timeout(minimum), Ok(minimum));
    assert_eq!(validate_shutdown_timeout(minimum + 1), Ok(minimum + 1));
}
