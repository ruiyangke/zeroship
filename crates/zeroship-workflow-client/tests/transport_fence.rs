//! What the transport will and will not send a service assertion over.
//!
//! The fence is `Transport::configuration`. Every check here drives it through
//! `Transport::validate_config`, which is the same predicate the clients use -
//! `WorkerCoordinator::new`, `ControlCoordinator::new`, `QueueDeploymentHolds`
//! and `SchemaBundles` all reach it - without keys or sockets.

use std::str::FromStr;
use zeroship_core::config::{PlaintextPeer, PlaintextPeers};
use zeroship_workflow_client::{Error, Options, Transport};

fn peers(origins: &[&str]) -> PlaintextPeers {
    origins
        .iter()
        .map(|origin| PlaintextPeer::from_str(origin).expect("a valid plaintext peer"))
        .collect()
}

fn options(origins: &[&str]) -> Options {
    Options {
        plaintext_peers: peers(origins),
        ..Options::default()
    }
}

/// THE DEFAULT POSTURE, and the property the named-peer list narrows rather
/// than removes. A deployment that configures nothing keeps this exactly.
///
/// Every origin below is plain HTTP to something that is not a literal
/// loopback address, and each is refused under `Options::default()`. If the
/// relaxation ever becomes general - a boolean, a private-range sniff, a
/// hostname resolved to loopback - this is the check that goes red.
#[test]
fn the_default_configuration_refuses_plaintext_to_every_non_loopback_host() {
    let default = Options::default();
    assert!(
        default.plaintext_peers.is_empty(),
        "the default names no plaintext peer"
    );
    for url in [
        "http://coordinator.example",
        "http://control:9090",
        "http://migrate-server:9091",
        // A NAME, not a literal address. Nothing resolves it, so a name that
        // happens to point at loopback today is still refused.
        "http://localhost:9095",
        "http://localhost",
        // Private ranges are not inferred. An operator opts in by saying so.
        "http://10.1.2.3:9090",
        "http://192.168.0.7:9090",
        "http://172.16.4.5:9090",
        "http://[fd00::1]:9090",
        // A literal that is not loopback, with no name involved at all.
        "http://192.0.2.1",
    ] {
        assert_eq!(
            Transport::validate_config(url, &default),
            Err(Error::InvalidConfig),
            "{url} must be refused by the default configuration"
        );
    }
    // The control: what the default DOES admit, so the refusals above are not
    // passing because everything is refused.
    for url in [
        "https://coordinator.example",
        "https://coordinator.example:9443/",
        "http://127.0.0.1:9095",
        "http://[::1]:9095",
    ] {
        Transport::validate_config(url, &default)
            .unwrap_or_else(|error| panic!("{url} is admitted by the default: {error}"));
    }
}

/// THE ONE-VARIABLE CONTROL. One process, ONE list, two peers: the origin the
/// operator named is admitted and the origin they did not is refused, judged by
/// the same `Options` value in the same call.
///
/// This is what an origin list buys over a per-setting boolean. With a boolean
/// the two arms would differ in the boolean itself - the thing under test - and
/// the comparison would establish nothing about narrowness.
#[test]
fn a_named_peer_is_admitted_while_an_unnamed_peer_of_the_same_process_is_refused() {
    let options = options(&["http://control:9090"]);

    Transport::validate_config("http://control:9090", &options)
        .expect("the named peer is admitted");
    assert_eq!(
        Transport::validate_config("http://migrate-server:9091", &options),
        Err(Error::InvalidConfig),
        "an origin this process did not name stays refused"
    );

    // The match is on the whole origin. A neighbour on the SAME host, and the
    // same host on its default port, are different origins and stay refused.
    for url in [
        "http://control:9091",
        "http://control",
        "http://control:9090.example",
    ] {
        assert_eq!(
            Transport::validate_config(url, &options),
            Err(Error::InvalidConfig),
            "{url} is not the origin that was named"
        );
    }

    // Naming a peer relaxes the scheme and nothing else: the rest of the origin
    // grammar is unchanged for a named peer.
    for url in [
        "http://user:secret@control:9090",
        "http://control:9090/prefix",
        "http://control:9090/?tenant=a",
        "http://control:9090/#fragment",
        "ftp://control:9090",
    ] {
        assert_eq!(
            Transport::validate_config(url, &options),
            Err(Error::InvalidConfig),
            "{url} is refused for a named peer as for any other"
        );
    }
}

/// Naming a peer does not lift the exchange bounds, and the bounds do not
/// depend on the peer being named. Without this, a fence test could pass over
/// options that were invalid for an unrelated reason.
#[test]
fn a_named_peer_is_still_bound_by_the_exchange_limits() {
    let admitted = options(&["http://control:9090"]);
    Transport::validate_config("http://control:9090", &admitted).expect("bounds are satisfied");

    for broken in [
        Options {
            timeout: std::time::Duration::ZERO,
            ..admitted.clone()
        },
        Options {
            max_request_bytes: 0,
            ..admitted.clone()
        },
        Options {
            max_response_bytes: 0,
            ..admitted
        },
    ] {
        assert_eq!(
            Transport::validate_config("http://control:9090", &broken),
            Err(Error::InvalidConfig),
            "a named peer does not excuse an empty bound"
        );
    }
}
