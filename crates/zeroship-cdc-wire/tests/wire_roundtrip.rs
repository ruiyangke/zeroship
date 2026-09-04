//! Round-trip corpus: every frame, every nested discriminant, both `Option`
//! tags.
//!
//! The proposal's acceptance suite asks for "exact hex goldens for one
//! `SubscribeRequest` and all eleven frame tags covering every nested
//! discriminant, both `Option` tags, a typed id, a full-width LSN and a nonempty
//! `CellValue`; decode and re-encode byte-identically". This file is the
//! decode/re-encode half and the coverage proof; the byte goldens belong with the
//! relay, where a fixture file can be compared against a live peer.
//!
//! **Every coverage assertion here compares against a `::ALL` slice rather than a
//! hand-written number.** A corpus that silently stops covering a variant someone
//! added is a corpus that prints exactly what a complete one prints.

mod support;

use std::collections::BTreeSet;

use support::{
    app_id, cluster_id, database_id, datastore_id, epoch, grant, relay_id, term, worker_id,
};
use zeroship_cdc_wire::{
    AppCursor, AppOutcome, AppRegistration, AppReset, CellValue, Change, ChangeIndex, ChangeOp,
    Envelope, Epoch, ExpectedBinding, Frame, FrameTag, Gap, GapReason, Heartbeat, Hello, Lsn,
    PrimeBatchId, Registered, RegistrationGeneration, RegistrationOutcome, RegistrationRejectCode,
    Relation, RelationGeneration, RelationPrime, ResetReason, Resync, Sequence, SubscribeRequest,
    SystemId, Timeline, Truncate, WIRE_VERSION,
};

fn envelope(seq: u64) -> Envelope {
    Envelope {
        app_id: app_id(7),
        grant_generation: grant(3),
        seq: Sequence::new(seq),
    }
}

/// One frame per tag, plus one extra per nested discriminant that would
/// otherwise go unexercised.
///
/// Long by construction: the coverage assertions below compare what it builds
/// against every `::ALL` slice, so splitting it into helpers would hide the one
/// property a reader checks - that each closed set appears here in full.
#[allow(clippy::too_many_lines)]
fn corpus() -> Vec<Frame> {
    let mut frames = vec![
        Frame::Hello(Hello {
            wire_version: WIRE_VERSION,
            relay_id: relay_id(1),
            leader_term: term(9),
            cluster_id: cluster_id(2),
            // Full-width, so a decoder that read a u32 or signed value fails here.
            systemid: SystemId::new(u64::MAX),
            timeline: Timeline::new(u32::MAX),
        }),
        Frame::RelationPrime(RelationPrime {
            prime_batch_id: PrimeBatchId::new(0),
            app_id: app_id(7),
            database_id: database_id(4),
            database_epoch: epoch(1),
            grant_generation: grant(3),
            relation_generation: RelationGeneration::new(11),
            collection: "patients".to_owned(),
            // A non-ASCII column name: the wire is UTF-8, not ASCII.
            columns: vec!["id".to_owned(), "note".to_owned(), "sso\u{df}".to_owned()],
        }),
        // An empty prime batch is valid: "count zero is a valid complete empty
        // batch".
        Frame::Registered(Registered {
            registration_generation: RegistrationGeneration::new(u64::MAX),
            outcomes: Vec::new(),
        }),
        Frame::Resync(Resync {
            prime_batch_id: PrimeBatchId::new(u64::MAX),
            prime_relation_count: 0,
            app_id: app_id(7),
            database_id: database_id(4),
            database_epoch: epoch(2),
            grant_generation: grant(3),
            reason: ResetReason::RingOverrun,
            accepted_cursor: Sequence::new(1_024),
        }),
        Frame::Relation(Relation {
            envelope: envelope(2),
            relation_generation: RelationGeneration::new(11),
            collection: "patients".to_owned(),
            columns: Vec::new(),
        }),
        Frame::Gap(Gap {
            envelope: envelope(3),
            collection: "patients".to_owned(),
            pk: vec![CellValue::Value(b"row-1".to_vec())],
            commit_lsn: Lsn::new(u64::MAX),
            change_index: ChangeIndex::new(u32::MAX),
            reason: GapReason::OversizeValue,
        }),
        Frame::Gap(Gap {
            envelope: envelope(4),
            collection: "patients".to_owned(),
            pk: Vec::new(),
            commit_lsn: Lsn::new(0),
            change_index: ChangeIndex::new(0),
            reason: GapReason::UnrepresentableType,
        }),
        Frame::Truncate(Truncate {
            envelope: envelope(5),
            collections: vec!["patients".to_owned(), "visits".to_owned()],
        }),
        Frame::Epoch(Epoch {
            envelope: envelope(6),
            database_epoch: epoch(i64::MAX as u64),
        }),
        Frame::Heartbeat(Heartbeat {
            datastore_id: datastore_id(5),
            // The integer behind the `0/16B6C50` text form the wire refuses.
            last_confirmed_lsn: Lsn::new(0x016B_6C50),
        }),
    ];

    // Every `ChangeOp`, and all three `CellValue` shapes in one row.
    for (index, op) in ChangeOp::ALL.iter().enumerate() {
        frames.push(Frame::Change(Change {
            envelope: envelope(100 + index as u64),
            relation_generation: RelationGeneration::new(11),
            op: *op,
            commit_lsn: Lsn::new(u64::MAX),
            change_index: ChangeIndex::new(u32::try_from(index).expect("fits")),
            pk: vec![CellValue::Value(b"row-1".to_vec())],
            values: vec![
                CellValue::Value(b"a nonempty cell".to_vec()),
                CellValue::Value(Vec::new()),
                CellValue::Null,
                CellValue::Unavailable,
            ],
        }));
    }

    // Every `ResetReason`, through the shared sequenced path.
    for (index, reason) in ResetReason::ALL.iter().enumerate() {
        frames.push(Frame::AppReset(AppReset {
            envelope: envelope(200 + index as u64),
            reason: *reason,
        }));
    }

    // Every `RegistrationOutcome` shape and every `RegistrationRejectCode`.
    let mut outcomes = vec![
        AppOutcome {
            app_id: app_id(1),
            outcome: RegistrationOutcome::Resumed,
        },
        AppOutcome {
            app_id: app_id(2),
            outcome: RegistrationOutcome::Reset(ResetReason::Initial),
        },
    ];
    for (index, code) in RegistrationRejectCode::ALL.iter().enumerate() {
        outcomes.push(AppOutcome {
            app_id: app_id(10 + u32::try_from(index).expect("fits")),
            outcome: RegistrationOutcome::Rejected(*code),
        });
    }
    frames.push(Frame::Registered(Registered {
        registration_generation: RegistrationGeneration::new(1),
        outcomes,
    }));

    frames
}

#[test]
fn corpus_covers_every_closed_set() {
    let frames = corpus();

    let tags: BTreeSet<FrameTag> = frames.iter().map(Frame::tag).collect();
    assert_eq!(
        tags,
        FrameTag::ALL.iter().copied().collect::<BTreeSet<_>>(),
        "the corpus must exercise all {} frame tags",
        FrameTag::ALL.len()
    );
    assert_eq!(FrameTag::ALL.len(), 11, "the proposal fixes eleven tags");

    let mut ops = BTreeSet::new();
    let mut reasons = BTreeSet::new();
    let mut gap_reasons = BTreeSet::new();
    let mut reject_codes = BTreeSet::new();
    let mut cell_shapes = BTreeSet::new();
    for frame in &frames {
        match frame {
            Frame::Change(change) => {
                ops.insert(change.op);
                for cell in change.pk.iter().chain(&change.values) {
                    cell_shapes.insert(match cell {
                        CellValue::Value(_) => "value",
                        CellValue::Null => "null",
                        CellValue::Unavailable => "unavailable",
                    });
                }
            }
            Frame::AppReset(reset) => {
                reasons.insert(reset.reason);
            }
            Frame::Resync(resync) => {
                reasons.insert(resync.reason);
            }
            Frame::Gap(gap) => {
                gap_reasons.insert(gap.reason);
            }
            Frame::Registered(registered) => {
                for outcome in &registered.outcomes {
                    if let RegistrationOutcome::Rejected(code) = outcome.outcome {
                        reject_codes.insert(code);
                    }
                }
            }
            _ => {}
        }
    }

    assert_eq!(ops, ChangeOp::ALL.iter().copied().collect::<BTreeSet<_>>());
    assert_eq!(
        reasons,
        ResetReason::ALL.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(ResetReason::ALL.len(), 11, "eleven reset reasons");
    assert_eq!(
        gap_reasons,
        GapReason::ALL.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        reject_codes,
        RegistrationRejectCode::ALL
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        RegistrationRejectCode::ALL.len(),
        7,
        "the proposal names exactly seven reject codes"
    );
    assert_eq!(
        cell_shapes.len(),
        3,
        "Value, Null and Unavailable must all appear"
    );
}

#[test]
fn every_frame_decodes_and_re_encodes_byte_identically() {
    let frames = corpus();
    assert!(frames.len() >= 25, "corpus floor: {} frames", frames.len());

    for frame in frames {
        let encoded = frame.encode().expect("encode");
        let decoded = Frame::decode(&encoded).expect("decode");
        assert_eq!(decoded, frame, "decoded frame differs");
        let re_encoded = decoded.encode().expect("re-encode");
        assert_eq!(re_encoded, encoded, "re-encode is not byte-identical");
    }
}

#[test]
fn the_length_prefix_counts_the_tag_and_payload() {
    let frame = Frame::Heartbeat(Heartbeat {
        datastore_id: datastore_id(5),
        last_confirmed_lsn: Lsn::new(1),
    });
    let encoded = frame.encode().expect("encode");
    let declared = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
    assert_eq!(
        declared as usize,
        encoded.len() - 4,
        "frame_len counts the tag and payload, not the prefix"
    );
    assert_eq!(encoded[4], FrameTag::Heartbeat.byte());
    assert_eq!(
        zeroship_cdc_wire::frame_length_prefix(&encoded).expect("prefix"),
        Some(encoded.len()),
        "frame_length_prefix returns the WHOLE frame size"
    );
}

#[test]
fn frame_length_prefix_needs_four_bytes_before_it_answers() {
    for len in 0..4 {
        let buf = vec![0u8; len];
        assert_eq!(
            zeroship_cdc_wire::frame_length_prefix(&buf).expect("short read is not an error"),
            None,
            "a {len}-byte buffer cannot carry a length yet"
        );
    }
}

#[test]
fn only_the_six_sequenced_tags_carry_an_envelope() {
    let sequenced: BTreeSet<FrameTag> = FrameTag::ALL
        .iter()
        .copied()
        .filter(|tag| tag.is_sequenced())
        .collect();
    assert_eq!(
        sequenced,
        [
            FrameTag::AppReset,
            FrameTag::Relation,
            FrameTag::Change,
            FrameTag::Gap,
            FrameTag::Truncate,
            FrameTag::Epoch,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
    );

    let mut checked = 0;
    for frame in corpus() {
        assert_eq!(
            frame.envelope().is_some(),
            frame.tag().is_sequenced(),
            "{:?} disagrees with its tag about the envelope",
            frame.tag()
        );
        checked += 1;
    }
    assert!(checked >= 25, "checked {checked} frames");
}

fn cursor() -> AppCursor {
    AppCursor {
        app_id: app_id(7),
        worker_id: worker_id(8),
        relay_id: relay_id(1),
        leader_term: term(9),
        database_id: database_id(4),
        database_epoch: epoch(2),
        grant_generation: grant(3),
        next_seq: Sequence::new(4_096),
    }
}

fn request() -> SubscribeRequest {
    SubscribeRequest {
        wire_version: WIRE_VERSION,
        cluster_id: cluster_id(2),
        worker_id: worker_id(8),
        term_permit: b"a control-signed permit".to_vec(),
        registration_generation: RegistrationGeneration::new(12),
        shard_index: 0,
        shard_count: 2,
        apps: vec![
            // Both `Option` tags, in one request.
            AppRegistration {
                app_id: app_id(7),
                expected_binding: ExpectedBinding {
                    database_id: database_id(4),
                    database_epoch: epoch(2),
                    grant_generation: grant(3),
                },
                cursor: Some(cursor()),
            },
            AppRegistration {
                app_id: app_id(8),
                expected_binding: ExpectedBinding {
                    database_id: database_id(4),
                    database_epoch: epoch(2),
                    grant_generation: grant(3),
                },
                cursor: None,
            },
        ],
    }
}

#[test]
fn subscribe_request_round_trips_byte_identically() {
    let original = request();
    let encoded = original.encode().expect("encode");
    let decoded = SubscribeRequest::decode(&encoded).expect("decode");
    assert_eq!(decoded, original);
    assert_eq!(decoded.encode().expect("re-encode"), encoded);
    assert_eq!(
        decoded
            .apps
            .iter()
            .filter(|app| app.cursor.is_some())
            .count(),
        1,
        "one Some and one None cursor must survive"
    );
}

#[test]
fn the_commitment_excludes_the_permit_and_covers_everything_else() {
    let base = request();
    let base_commitment = base.commitment().expect("commitment");

    // Changing only the permit must not move the commitment.
    let mut other_permit = request();
    other_permit.term_permit = b"a different permit entirely".to_vec();
    assert_eq!(
        other_permit.commitment().expect("commitment"),
        base_commitment,
        "the permit is not in its own preimage"
    );

    // An empty permit must not move it either: the zero-length encoding IS the
    // canonical form, so a request with no permit commits identically.
    let mut empty_permit = request();
    empty_permit.term_permit = Vec::new();
    assert_eq!(
        empty_permit.commitment().expect("commitment"),
        base_commitment
    );

    // Every other field must move it. One mutation per field, so a field the
    // encoder forgot to write is a failure here rather than a silent hole.
    let mut mutations: Vec<SubscribeRequest> = Vec::new();
    let mut m = request();
    m.cluster_id = cluster_id(3);
    mutations.push(m);
    let mut m = request();
    m.worker_id = worker_id(9);
    mutations.push(m);
    let mut m = request();
    m.registration_generation = RegistrationGeneration::new(13);
    mutations.push(m);
    let mut m = request();
    m.shard_index = 1;
    mutations.push(m);
    let mut m = request();
    m.shard_count = 3;
    mutations.push(m);
    let mut m = request();
    m.apps.truncate(1);
    mutations.push(m);
    let mut m = request();
    m.apps[0].cursor = None;
    mutations.push(m);
    let mut m = request();
    m.apps[0].expected_binding.database_epoch = epoch(3);
    mutations.push(m);
    let mut m = request();
    if let Some(c) = m.apps[0].cursor.as_mut() {
        c.next_seq = Sequence::new(4_097);
    }
    mutations.push(m);

    assert_eq!(mutations.len(), 9, "nine non-permit mutations");
    for (index, mutated) in mutations.iter().enumerate() {
        assert_ne!(
            mutated.commitment().expect("commitment"),
            base_commitment,
            "mutation {index} did not change the commitment"
        );
    }
}

#[test]
fn hashing_the_permit_into_its_own_preimage_would_not_match() {
    // The negative control the proposal names: a commitment computed over the
    // FULL bytes (permit included) must differ from the real one, or the
    // exclusion is not doing anything.
    let base = request();
    let full = base.encode().expect("encode");
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(
        &mut hasher,
        zeroship_cdc_wire::limits::PERMIT_COMMITMENT_DOMAIN,
    );
    sha2::Digest::update(&mut hasher, &full);
    let permit_in_preimage: [u8; 32] = sha2::Digest::finalize(hasher).into();
    assert_ne!(permit_in_preimage, base.commitment().expect("commitment"));
}
