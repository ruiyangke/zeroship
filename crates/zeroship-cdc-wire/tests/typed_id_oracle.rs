//! The differential oracle for the duplicated base62 parse, and the pin that
//! keeps the wire's `ChangeOp` one-to-one with the broker's.
//!
//! `zeroship-cdc-wire` deliberately does not depend on `zeroship-core` (see its
//! `Cargo.toml`: that crate's normal dependencies include an HTTP client, and
//! this one's stated property is that it performs no I/O). The cost is a second
//! implementation of the platform's canonical typed-id parse. This file is what
//! bounds that cost: both parsers run over one corpus and must agree on every
//! input, so a drift shows up here rather than as two services disagreeing about
//! whether an id is valid.
//!
//! `zeroship-core` is a DEV-dependency only, which is why this comparison is
//! possible without putting a runtime in the shipped closure.

use zeroship_cdc_wire::{AppId, DatastoreId, WorkerId};
use zeroship_core::typed_id;

/// Every case is `(input, prefix)`. Both parsers see the same pair.
fn corpus() -> Vec<(String, &'static str)> {
    let mut cases: Vec<(String, &'static str)> = Vec::new();

    // Ids minted by the platform encoder itself. If the two disagree about one
    // of these, the wire cannot carry a real id.
    for _ in 0..64 {
        cases.push((typed_id::generate("app"), "app"));
    }

    // Both ends of the canonical range, derived rather than hardcoded.
    for uuid in [
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "01890a5d-ac96-774b-bcce-b302099a8057",
    ] {
        cases.push((
            typed_id::from_uuid_string("app", uuid).expect("encodes"),
            "app",
        ));
    }

    // 36^25 - 1 is above 2^128: legal characters at the right width that no
    // encoder ever produced, because the value they spell is not a UUID.
    cases.push(("app_zzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(), "app"));
    cases.push(("app_zzzzzzzzzzzzzzzzzzzzzzzzy".to_owned(), "app"));
    // One past the largest UUID - the tightest overflow the decode must refuse.
    cases.push(("app_f5lxx1zz5pnorynqglhzmsp34".to_owned(), "app"));

    // Wrong prefix, right body.
    let good = typed_id::generate("app");
    let body = good.split_once('_').expect("has a body").1.to_owned();
    cases.push((format!("usr_{body}"), "app"));
    cases.push((format!("ap_{body}"), "app"));
    cases.push((format!("appp_{body}"), "app"));
    cases.push((format!("APP_{body}"), "app"));

    // Wrong length.
    cases.push((format!("app_{}", &body[..21]), "app"));
    cases.push((format!("app_{body}0"), "app"));
    cases.push(("app_".to_owned(), "app"));

    // Shape.
    cases.push(("app".to_owned(), "app"));
    cases.push((String::new(), "app"));
    cases.push(((*good).to_string(), "app"));
    cases.push((format!("app_{}_x", &body[..20]), "app"));

    // Out-of-alphabet bytes, all at length 22.
    for bad in ['-', '_', '+', '/', ' ', '\u{e9}'] {
        let mut mangled: String = body.clone();
        mangled.pop();
        mangled.push(bad);
        cases.push((format!("app_{mangled}"), "app"));
    }

    // Over the 64-byte bound.
    cases.push((format!("app_{}", "0".repeat(80)), "app"));

    // The two-letter prefix, which is the spelling the decoupling proposal
    // writes for a Datastore and the one this crate flags as contested.
    cases.push((typed_id::generate("ds"), "ds"));
    cases.push((typed_id::generate("wrk"), "wrk"));

    cases
}

fn wire_accepts(input: &str, prefix: &str) -> bool {
    match prefix {
        "app" => AppId::parse(input).is_ok(),
        "ds" => DatastoreId::parse(input).is_ok(),
        "wrk" => WorkerId::parse(input).is_ok(),
        other => panic!("no wire id type for prefix {other}"),
    }
}

#[test]
fn both_parsers_agree_on_every_input() {
    let cases = corpus();
    assert!(
        cases.len() >= 85,
        "corpus floor: the oracle ruled on {} inputs",
        cases.len()
    );

    let mut accepted = 0usize;
    let mut rejected = 0usize;
    for (input, prefix) in &cases {
        let wire = wire_accepts(input, prefix);
        let platform = typed_id::parse_with_prefix(input, prefix).is_ok();
        assert_eq!(
            wire, platform,
            "disagreement on {input:?} with prefix {prefix:?}: wire={wire}, platform={platform}"
        );
        if wire {
            accepted += 1;
        } else {
            rejected += 1;
        }
    }

    // An oracle that agreed because both sides rejected everything would prove
    // nothing. Both arms must be non-trivially populated.
    assert!(accepted >= 65, "accepted {accepted} inputs");
    assert!(rejected >= 15, "rejected {rejected} inputs");
}

#[test]
fn an_accepted_id_round_trips_to_the_same_text() {
    let mut checked = 0;
    for _ in 0..32 {
        let minted = typed_id::generate("app");
        let parsed = AppId::parse(&minted).expect("platform-minted id must parse");
        assert_eq!(parsed.as_str(), minted);
        assert_eq!(parsed.to_string(), minted);
        checked += 1;
    }
    assert_eq!(checked, 32);
}

#[test]
fn the_prefixes_this_crate_pins_are_the_ones_the_proposals_write() {
    // `app` is the platform's, and must not be re-spelled here.
    assert_eq!(AppId::PREFIX, typed_id::APP_PREFIX);
    // The other five have no platform constant yet. Pinned so the entity work
    // adopting them is a compile-visible decision rather than a coincidence.
    assert_eq!(DatastoreId::PREFIX, "ds");
    assert_eq!(WorkerId::PREFIX, "wrk");
    assert_eq!(zeroship_cdc_wire::DatabaseId::PREFIX, "dbs");
    assert_eq!(zeroship_cdc_wire::ClusterId::PREFIX, "clu");
    assert_eq!(zeroship_cdc_wire::RelayId::PREFIX, "rly");
}

#[test]
fn the_wire_change_op_is_one_to_one_with_the_broker_change_op() {
    use zeroship_cdc_wire::ChangeOp as WireOp;
    use zeroship_core::change_event::ChangeOp as BrokerOp;

    // This match is the pin: it is exhaustive over the BROKER's enum, so adding
    // a variant there fails to compile here rather than silently producing a
    // change kind the wire cannot carry.
    fn to_wire(op: BrokerOp) -> WireOp {
        match op {
            BrokerOp::Insert => WireOp::Insert,
            BrokerOp::Update => WireOp::Update,
            BrokerOp::Delete => WireOp::Delete,
        }
    }

    // And this one is exhaustive over the WIRE's enum, in the other direction.
    fn to_broker(op: WireOp) -> BrokerOp {
        match op {
            WireOp::Insert => BrokerOp::Insert,
            WireOp::Update => BrokerOp::Update,
            WireOp::Delete => BrokerOp::Delete,
        }
    }

    assert_eq!(WireOp::ALL.len(), 3);
    let mut checked = 0;
    for op in WireOp::ALL {
        assert_eq!(to_wire(to_broker(*op)), *op);
        checked += 1;
    }
    assert_eq!(checked, 3);
    assert_eq!(to_broker(WireOp::Insert).as_str(), "insert");
    assert_eq!(to_broker(WireOp::Update).as_str(), "update");
    assert_eq!(to_broker(WireOp::Delete).as_str(), "delete");
}
