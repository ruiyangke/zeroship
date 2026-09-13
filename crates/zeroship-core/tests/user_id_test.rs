use std::collections::BTreeMap;

use zeroship_core::UserId;

#[test]
fn user_id_is_canonical_at_construction_and_on_the_wire() {
    let minted = UserId::mint();
    assert!(minted.as_str().starts_with("usr_"));

    let parsed = UserId::parse(minted.as_str()).expect("minted user id parses");
    assert_eq!(parsed, minted);

    let json = serde_json::to_string(&minted).expect("user id serializes");
    assert_eq!(json, format!("\"{}\"", minted.as_str()));
    assert_eq!(
        serde_json::from_str::<UserId>(&json).expect("canonical user id deserializes"),
        minted
    );
}

#[test]
fn user_id_refuses_uuid_and_other_typed_id_wire_values() {
    for raw in [
        "0191e7a2-b3c4-4d5e-8f90-123456789abc",
        "app_0000000000000000000000",
        "usr_000000000000000000000",
        "usr_00000000000000000000000",
    ] {
        assert!(UserId::parse(raw).is_err(), "{raw} must be refused");
        let json = serde_json::to_string(raw).expect("fixture serializes");
        assert!(
            serde_json::from_str::<UserId>(&json).is_err(),
            "{raw} must be refused on the wire"
        );
    }
}

#[test]
fn user_id_is_a_strict_serde_map_key() {
    let id = UserId::mint();
    let mut users = BTreeMap::new();
    users.insert(id.clone(), "active");

    let json = serde_json::to_string(&users).expect("map serializes");
    assert_eq!(
        serde_json::from_str::<BTreeMap<UserId, &str>>(&json).expect("map round trips"),
        users
    );

    assert!(serde_json::from_str::<BTreeMap<UserId, &str>>(
        "{\"0191e7a2-b3c4-4d5e-8f90-123456789abc\":\"active\"}"
    )
    .is_err());
}
