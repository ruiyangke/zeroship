//! The rejection corpus.
//!
//! "A zero length, a length over `max_frame_bytes`, invalid UTF-8, trailing
//! payload bytes or an unknown tag is a fatal protocol violation that closes the
//! response", and the acceptance suite adds "Boolean `0x02`, a noncanonical id,
//! an out-of-domain generation, a false vector count, truncation, trailing
//! bytes".
//!
//! Each case asserts the EXACT error variant. Asserting only `is_err()` would
//! pass for a decoder that rejected everything for the wrong reason, and the
//! reason is the part a relay operator sees.

mod support;

use support::{
    app_id, cluster_id, database_id, datastore_id, epoch, grant, relay_id, term, worker_id,
};
use zeroship_cdc_wire::{
    AppCursor, AppOutcome, AppRegistration, CellValue, Change, ChangeIndex, ChangeOp, DecodeError,
    EncodeError, Envelope, Epoch, ExpectedBinding, Frame, FrameTag, Heartbeat, Hello, Lsn,
    PrimeBatchId, Registered, RegistrationGeneration, RegistrationOutcome, Relation,
    RelationGeneration, ResetReason, Sequence, SubscribeRequest, SystemId, Timeline, Truncate,
    MAX_APPS_PER_SHARD, MAX_CELL_BYTES, MAX_FRAME_BYTES, MAX_REQUEST_BYTES, MAX_TYPED_ID_BYTES,
    WIRE_VERSION,
};

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("marker {needle:?} not found in the encoded frame"))
}

fn envelope(seq: u64) -> Envelope {
    Envelope {
        app_id: app_id(7),
        grant_generation: grant(3),
        seq: Sequence::new(seq),
    }
}

fn heartbeat() -> Frame {
    Frame::Heartbeat(Heartbeat {
        datastore_id: datastore_id(1),
        last_confirmed_lsn: Lsn::new(42),
    })
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[test]
fn a_zero_length_frame_is_fatal() {
    let buf = [0u8, 0, 0, 0];
    assert_eq!(
        Frame::decode(&buf).unwrap_err(),
        DecodeError::ZeroFrameLength
    );
    assert_eq!(
        zeroship_cdc_wire::frame_length_prefix(&buf).unwrap_err(),
        DecodeError::ZeroFrameLength
    );
}

#[test]
fn a_length_one_over_the_maximum_is_fatal() {
    let declared = MAX_FRAME_BYTES + 1;
    let mut buf = declared.to_be_bytes().to_vec();
    buf.push(FrameTag::Heartbeat.byte());
    assert_eq!(
        Frame::decode(&buf).unwrap_err(),
        DecodeError::FrameTooLarge { declared }
    );

    // The control: exactly the maximum passes the LENGTH check and fails later,
    // on the body, so the boundary is `>` and not `>=`.
    let at_max = MAX_FRAME_BYTES.to_be_bytes().to_vec();
    assert!(matches!(
        zeroship_cdc_wire::frame_length_prefix(&at_max),
        Ok(Some(_))
    ));
}

#[test]
fn an_unknown_tag_is_its_own_error() {
    for tag in [0x00u8, 0x0c, 0xff] {
        let mut buf = 1u32.to_be_bytes().to_vec();
        buf.push(tag);
        assert_eq!(
            Frame::decode(&buf).unwrap_err(),
            DecodeError::UnknownFrameTag { tag },
            "tag {tag:#04x}"
        );
    }
    // The control: every fixed tag maps back.
    for tag in FrameTag::ALL {
        assert_eq!(FrameTag::from_byte(tag.byte()), Ok(*tag));
    }
}

#[test]
fn trailing_bytes_and_truncation_are_both_fatal() {
    let encoded = heartbeat().encode().expect("encode");

    let mut long = encoded.clone();
    long.push(0);
    assert_eq!(
        Frame::decode(&long).unwrap_err(),
        DecodeError::TrailingBytes { remaining: 1 }
    );

    let short = &encoded[..encoded.len() - 1];
    assert_eq!(
        Frame::decode(short).unwrap_err(),
        DecodeError::Truncated {
            needed: encoded.len(),
            available: encoded.len() - 1,
        }
    );

    // Trailing bytes inside a payload, with the length prefix agreeing, must
    // also fail: the frame is well-framed and still wrong.
    let mut payload = encoded[5..].to_vec();
    payload.push(0);
    assert_eq!(
        Frame::decode_payload(FrameTag::Heartbeat, &payload).unwrap_err(),
        DecodeError::TrailingBytes { remaining: 1 }
    );
}

// ---------------------------------------------------------------------------
// Scalars and discriminants
// ---------------------------------------------------------------------------

#[test]
fn an_out_of_domain_generation_is_refused_before_it_is_stored() {
    let frame = Frame::Epoch(Epoch {
        envelope: envelope(1),
        database_epoch: epoch(5),
    });
    let encoded = frame.encode().expect("encode");
    let offset = encoded.len() - 8;

    for (label, value) in [
        ("zero", 0u64),
        ("i64::MAX + 1", (i64::MAX as u64) + 1),
        ("u64::MAX", u64::MAX),
    ] {
        let mut patched = encoded.clone();
        patched[offset..].copy_from_slice(&value.to_be_bytes());
        assert_eq!(
            Frame::decode(&patched).unwrap_err(),
            DecodeError::OutOfDomain {
                kind: "DatabaseEpoch"
            },
            "{label} must be refused"
        );
    }

    // Both ends of the domain are accepted, so the refusal is a domain and not a
    // blanket rejection.
    for value in [1u64, i64::MAX as u64] {
        let mut patched = encoded.clone();
        patched[offset..].copy_from_slice(&value.to_be_bytes());
        assert!(Frame::decode(&patched).is_ok(), "{value} must be accepted");
    }
}

#[test]
fn a_nested_discriminant_outside_its_set_is_fatal() {
    let frame = Frame::Change(Change {
        envelope: envelope(1),
        relation_generation: RelationGeneration::new(1),
        op: ChangeOp::Insert,
        commit_lsn: Lsn::new(1),
        change_index: ChangeIndex::new(0),
        pk: Vec::new(),
        values: Vec::new(),
    });
    let encoded = frame.encode().expect("encode");
    // With both cell vectors empty the tail is fixed: op(1) + commit_lsn(8) +
    // change_index(4) + pk count(4) + values count(4) = 21 bytes.
    let op_offset = encoded.len() - 21;
    assert_eq!(
        encoded[op_offset],
        ChangeOp::Insert.discriminant(),
        "the op byte is where the declaration order says it is"
    );
    let mut patched = encoded;
    patched[op_offset] = 0x00;
    assert_eq!(
        Frame::decode(&patched).unwrap_err(),
        DecodeError::InvalidDiscriminant {
            kind: "ChangeOp",
            value: 0x00
        },
        "0x00 is never a valid nested discriminant"
    );

    // Every fixed discriminant maps back, and none is zero.
    for op in ChangeOp::ALL {
        assert_ne!(op.discriminant(), 0x00);
        assert_eq!(ChangeOp::from_discriminant(op.discriminant()), Ok(*op));
    }
    for reason in ResetReason::ALL {
        assert_ne!(reason.discriminant(), 0x00);
        assert_eq!(
            ResetReason::from_discriminant(reason.discriminant()),
            Ok(*reason)
        );
    }
    assert_eq!(
        ResetReason::from_discriminant(0x0c).unwrap_err(),
        DecodeError::InvalidDiscriminant {
            kind: "ResetReason",
            value: 0x0c
        }
    );
}

#[test]
fn a_boolean_other_than_zero_or_one_is_fatal() {
    // The only Boolean on this wire is the `Option` tag on `AppCursor`.
    let request = SubscribeRequest {
        wire_version: WIRE_VERSION,
        cluster_id: cluster_id(1),
        worker_id: worker_id(1),
        term_permit: Vec::new(),
        registration_generation: RegistrationGeneration::new(1),
        shard_index: 0,
        shard_count: 1,
        apps: vec![AppRegistration {
            app_id: app_id(1),
            expected_binding: ExpectedBinding {
                database_id: database_id(1),
                database_epoch: epoch(1),
                grant_generation: grant(1),
            },
            cursor: None,
        }],
    };
    let encoded = request.encode().expect("encode");
    let offset = encoded.len() - 1;
    assert_eq!(encoded[offset], 0x00, "the None tag is the last byte");
    let mut patched = encoded;
    patched[offset] = 0x02;
    assert_eq!(
        SubscribeRequest::decode(&patched).unwrap_err(),
        DecodeError::InvalidBoolean { value: 0x02 }
    );
}

#[test]
fn invalid_utf8_in_a_string_is_fatal_and_leaks_nothing() {
    let frame = Frame::Relation(Relation {
        envelope: envelope(1),
        relation_generation: RelationGeneration::new(1),
        collection: "MARKER".to_owned(),
        columns: Vec::new(),
    });
    let encoded = frame.encode().expect("encode");
    let offset = find(&encoded, b"MARKER");
    let mut patched = encoded;
    patched[offset] = 0xff;
    let err = Frame::decode(&patched).unwrap_err();
    assert_eq!(err, DecodeError::InvalidUtf8);
    // The rendered error carries no payload bytes.
    assert!(!err.to_string().contains("MARKE"));
}

// ---------------------------------------------------------------------------
// Typed ids
// ---------------------------------------------------------------------------

#[test]
fn a_noncanonical_id_is_fatal() {
    let encoded = heartbeat().encode().expect("encode");
    let body = support::body(1);
    let offset = find(&encoded, body.as_bytes());

    // 62^22 - 1 is greater than 2^128, so this is 22 valid base62 characters
    // that no encoder can have produced.
    let mut patched = encoded.clone();
    patched[offset..offset + 22].copy_from_slice(b"ZZZZZZZZZZZZZZZZZZZZZZ");
    assert_eq!(
        Frame::decode(&patched).unwrap_err(),
        DecodeError::MalformedTypedId {
            expected_prefix: "ds"
        }
    );

    // A character outside the alphabet, same length.
    let mut patched = encoded.clone();
    patched[offset] = b'-';
    assert_eq!(
        Frame::decode(&patched).unwrap_err(),
        DecodeError::MalformedTypedId {
            expected_prefix: "ds"
        }
    );

    // The control: unpatched, it decodes.
    assert!(Frame::decode(&encoded).is_ok());
}

#[test]
fn a_wrong_prefix_is_fatal_even_when_the_body_is_canonical() {
    let encoded = heartbeat().encode().expect("encode");
    let offset = find(&encoded, b"ds_");
    let mut patched = encoded;
    patched[offset..offset + 2].copy_from_slice(b"xy");
    assert_eq!(
        Frame::decode(&patched).unwrap_err(),
        DecodeError::MalformedTypedId {
            expected_prefix: "ds"
        }
    );
}

#[test]
fn an_over_long_id_length_is_refused_before_the_bytes_are_read() {
    let encoded = heartbeat().encode().expect("encode");
    let body = support::body(1);
    let id_start = find(&encoded, b"ds_");
    let len_offset = id_start - 4;
    let declared = u32::try_from(MAX_TYPED_ID_BYTES + 1).expect("fits");
    let mut patched = encoded;
    patched[len_offset..len_offset + 4].copy_from_slice(&declared.to_be_bytes());
    assert_eq!(
        Frame::decode(&patched).unwrap_err(),
        DecodeError::TypedIdTooLong { declared },
        "the bound is checked before the {} id bytes are taken",
        body.len() + 3
    );
}

// ---------------------------------------------------------------------------
// Counts, cells and caps
// ---------------------------------------------------------------------------

#[test]
fn a_vector_count_larger_than_the_input_is_refused_before_allocation() {
    let frame = Frame::Truncate(Truncate {
        envelope: envelope(1),
        collections: Vec::new(),
    });
    let encoded = frame.encode().expect("encode");
    let offset = encoded.len() - 4;
    assert_eq!(
        &encoded[offset..],
        &0u32.to_be_bytes(),
        "the empty collection count is the last field"
    );

    for count in [1u32, 1_000, u32::MAX] {
        let mut patched = encoded.clone();
        patched[offset..].copy_from_slice(&count.to_be_bytes());
        assert_eq!(
            Frame::decode(&patched).unwrap_err(),
            DecodeError::CountExceedsInput {
                kind: "collections",
                count
            },
            "a count of {count} over zero remaining bytes"
        );
    }
}

#[test]
fn a_cell_over_the_budget_is_refused_on_both_sides() {
    // Encode side: the producer must emit a keyed Gap instead, so the encoder
    // refuses rather than emitting an oversize cell.
    let oversize = Frame::Change(Change {
        envelope: envelope(1),
        relation_generation: RelationGeneration::new(1),
        op: ChangeOp::Insert,
        commit_lsn: Lsn::new(1),
        change_index: ChangeIndex::new(0),
        pk: Vec::new(),
        values: vec![CellValue::Value(vec![0u8; MAX_CELL_BYTES as usize + 1])],
    });
    assert_eq!(
        oversize.encode().unwrap_err(),
        EncodeError::CellTooLarge {
            len: MAX_CELL_BYTES as usize + 1
        }
    );

    // Decode side: a peer that ignored that rule is refused before its bytes are
    // taken.
    let small = Frame::Change(Change {
        envelope: envelope(1),
        relation_generation: RelationGeneration::new(1),
        op: ChangeOp::Insert,
        commit_lsn: Lsn::new(1),
        change_index: ChangeIndex::new(0),
        pk: Vec::new(),
        values: vec![CellValue::Value(b"MARKERVAL".to_vec())],
    });
    let encoded = small.encode().expect("encode");
    let value_offset = find(&encoded, b"MARKERVAL");
    let declared = MAX_CELL_BYTES + 1;
    let mut patched = encoded;
    patched[value_offset - 4..value_offset].copy_from_slice(&declared.to_be_bytes());
    assert_eq!(
        Frame::decode(&patched).unwrap_err(),
        DecodeError::CellTooLarge { declared }
    );
}

#[test]
fn a_frame_over_the_budget_is_refused_by_the_encoder() {
    // Three cells at the per-cell maximum exceed the per-frame maximum, so the
    // two limits are independent and both are enforced.
    let cells: Vec<CellValue> = (0..3)
        .map(|_| CellValue::Value(vec![0u8; MAX_CELL_BYTES as usize]))
        .collect();
    let frame = Frame::Change(Change {
        envelope: envelope(1),
        relation_generation: RelationGeneration::new(1),
        op: ChangeOp::Insert,
        commit_lsn: Lsn::new(1),
        change_index: ChangeIndex::new(0),
        pk: Vec::new(),
        values: cells,
    });
    assert!(matches!(
        frame.encode().unwrap_err(),
        EncodeError::FrameTooLarge { .. }
    ));
}

#[test]
fn the_shard_cap_is_enforced_on_both_sides() {
    let over = MAX_APPS_PER_SHARD as usize + 1;

    let registered = Frame::Registered(Registered {
        registration_generation: RegistrationGeneration::new(1),
        outcomes: (0..over)
            .map(|index| AppOutcome {
                app_id: app_id(u32::try_from(index).expect("fits")),
                outcome: RegistrationOutcome::Resumed,
            })
            .collect(),
    });
    assert_eq!(
        registered.encode().unwrap_err(),
        EncodeError::TooManyApps { count: over }
    );

    // Decode side: a count over the cap, with enough filler for the
    // pre-allocation check to pass, so `TooManyApps` is what fires and not
    // `CountExceedsInput`.
    let mut payload = RegistrationGeneration::new(1).get().to_be_bytes().to_vec();
    let count = MAX_APPS_PER_SHARD + 1;
    payload.extend_from_slice(&count.to_be_bytes());
    payload.extend(std::iter::repeat_n(0u8, over * 5));
    assert_eq!(
        Frame::decode_payload(FrameTag::Registered, &payload).unwrap_err(),
        DecodeError::TooManyApps { count }
    );
}

#[test]
fn a_body_over_the_request_cap_is_refused_before_it_is_parsed() {
    let buf = vec![0u8; MAX_REQUEST_BYTES as usize + 1];
    assert_eq!(
        SubscribeRequest::decode(&buf).unwrap_err(),
        DecodeError::RequestTooLarge { len: buf.len() },
        "the cap is checked before the first field"
    );
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

#[test]
fn no_version_but_the_supported_one_decodes() {
    let hello = Frame::Hello(Hello {
        wire_version: WIRE_VERSION,
        relay_id: relay_id(1),
        leader_term: term(1),
        cluster_id: cluster_id(1),
        systemid: SystemId::new(1),
        timeline: Timeline::new(1),
    });
    let encoded = hello.encode().expect("encode");
    // The version is the first two payload bytes: prefix (4) + tag (1).
    for declared in [0u16, WIRE_VERSION + 1, u16::MAX] {
        let mut patched = encoded.clone();
        patched[5..7].copy_from_slice(&declared.to_be_bytes());
        assert_eq!(
            Frame::decode(&patched).unwrap_err(),
            DecodeError::UnsupportedWireVersion { declared },
            "Hello must not best-effort decode version {declared}"
        );
    }

    let request = SubscribeRequest {
        wire_version: WIRE_VERSION,
        cluster_id: cluster_id(1),
        worker_id: worker_id(1),
        term_permit: Vec::new(),
        registration_generation: RegistrationGeneration::new(1),
        shard_index: 0,
        shard_count: 1,
        apps: Vec::new(),
    };
    let encoded = request.encode().expect("encode");
    let mut patched = encoded;
    patched[0..2].copy_from_slice(&2u16.to_be_bytes());
    assert_eq!(
        SubscribeRequest::decode(&patched).unwrap_err(),
        DecodeError::UnsupportedWireVersion { declared: 2 }
    );
}

#[test]
fn a_prime_batch_and_a_cursor_survive_the_frames_that_carry_them() {
    // A control for the negatives above: the shapes they patch are otherwise
    // valid, so a decoder that rejected everything would fail here.
    let cursor = AppCursor {
        app_id: app_id(1),
        worker_id: worker_id(1),
        relay_id: relay_id(1),
        leader_term: term(1),
        database_id: database_id(1),
        database_epoch: epoch(1),
        grant_generation: grant(1),
        next_seq: Sequence::new(0),
    };
    let request = SubscribeRequest {
        wire_version: WIRE_VERSION,
        cluster_id: cluster_id(1),
        worker_id: worker_id(1),
        term_permit: b"permit".to_vec(),
        registration_generation: RegistrationGeneration::new(1),
        shard_index: 0,
        shard_count: 1,
        apps: vec![AppRegistration {
            app_id: app_id(1),
            expected_binding: ExpectedBinding {
                database_id: database_id(1),
                database_epoch: epoch(1),
                grant_generation: grant(1),
            },
            cursor: Some(cursor),
        }],
    };
    let encoded = request.encode().expect("encode");
    assert_eq!(SubscribeRequest::decode(&encoded).expect("decode"), request);

    let prime = Frame::RelationPrime(zeroship_cdc_wire::RelationPrime {
        prime_batch_id: PrimeBatchId::new(1),
        app_id: app_id(1),
        database_id: database_id(1),
        database_epoch: epoch(1),
        grant_generation: grant(1),
        relation_generation: RelationGeneration::new(1),
        collection: "patients".to_owned(),
        columns: vec!["id".to_owned()],
    });
    let encoded = prime.encode().expect("encode");
    assert_eq!(Frame::decode(&encoded).expect("decode"), prime);
}
