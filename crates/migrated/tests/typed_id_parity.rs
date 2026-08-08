//! The base62/UUIDv7 id encoding exists TWICE in this tree, and both copies
//! are on the same wire.
//!
//!   `zeroship_core::typed_id`                     the platform's typed ids
//!   `zero_migrate_ir::id` (third_party/zero-migrate)  the engine's MigrationId
//!
//! The engine's copy opens by calling the encoding a wire contract and citing a
//! `tests/core_id_parity.rs` drift guard that "asserts these copies stay
//! identical to core while both crates coexist in-tree". No such file exists in
//! the engine repository, and it CANNOT: the engine workspace has no `core`
//! crate to compare against, so the condition the comment names is false there.
//!
//! It is true HERE. This repository is the only place both copies coexist, so
//! this is where the guard can actually run. A divergence would surface as ids
//! that round-trip differently between `zeroship_core` and `MigrationId`.
//!
//! What this does NOT cover: the engine's `parse`/`validate` prefix helpers,
//! and anything that reads an id without going through these two functions.
//! Only the encode/decode pair is pinned.

use uuid::Uuid;

/// Fixed vectors first (all-zero, all-ones, low bit) so a failure names a value
/// that is reproducible without a seed, then a wide sweep of real v7 ids.
fn vectors() -> Vec<Uuid> {
    let mut ids = vec![
        Uuid::from_bytes([0u8; 16]),
        Uuid::from_bytes([0xff; 16]),
        Uuid::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
    ];
    for _ in 0..20_000 {
        ids.push(Uuid::now_v7());
    }
    ids
}

#[test]
fn core_and_engine_encode_ids_identically() {
    for id in vectors() {
        let core = zeroship_core::typed_id::uuid_to_base62(&id);
        let engine = zero_migrate_ir::id::uuid_to_base62(&id);
        assert_eq!(
            core, engine,
            "typed-id ENCODE diverged between zeroship_core and zero_migrate_ir for {id}"
        );
    }
}

#[test]
fn core_and_engine_decode_each_others_ids() {
    // Cross-decode, not just same-side round-trip: a shared bug in one
    // direction would still let each side round-trip its own output.
    for id in vectors() {
        let core = zeroship_core::typed_id::uuid_to_base62(&id);
        let engine = zero_migrate_ir::id::uuid_to_base62(&id);

        let core_reads_engine = zeroship_core::typed_id::base62_to_uuid(&engine)
            .expect("core must parse an engine-encoded id");
        let engine_reads_core = zero_migrate_ir::id::base62_to_uuid(&core)
            .expect("engine must parse a core-encoded id");

        assert_eq!(core_reads_engine, id, "core misread an engine-encoded id");
        assert_eq!(engine_reads_core, id, "engine misread a core-encoded id");
    }
}

#[test]
fn both_sides_use_the_same_alphabet_in_the_same_order() {
    // The alphabet is ordered so lexicographic sort matches numeric sort on the
    // timestamp high bits. A reordered-but-complete alphabet keeps every id
    // parseable on its own side and silently breaks that ordering property, so
    // pin it directly rather than inferring it from round-trip success.
    // The low sweep is the part that does the work: n in 0..62 encodes to 21
    // zeros plus the n-th alphabet character, so the sorted-ness assertion
    // below reads the alphabet's order directly, one character per value. A
    // sweep of only high-bit values (`n << 80`) never lands on most digits and
    // silently misses a reordering -- verified by swapping the last two
    // characters in BOTH copies, which that sweep alone reports as clean.
    let mut values: Vec<u128> = (0u128..62).chain((0u128..512).map(|n| n << 80)).collect();
    values.sort_unstable();
    values.dedup();
    let ascending: Vec<Uuid> = values
        .into_iter()
        .map(|n| Uuid::from_bytes(n.to_be_bytes()))
        .collect();

    let core: Vec<String> = ascending
        .iter()
        .map(zeroship_core::typed_id::uuid_to_base62)
        .collect();
    let engine: Vec<String> = ascending
        .iter()
        .map(zero_migrate_ir::id::uuid_to_base62)
        .collect();

    assert_eq!(core, engine, "encoded sequences differ");

    let mut sorted = core.clone();
    sorted.sort();
    assert_eq!(
        core, sorted,
        "lexicographic order stopped matching numeric order: the alphabet is no \
         longer ascending, which breaks UUIDv7 sort order for both crates"
    );
}
