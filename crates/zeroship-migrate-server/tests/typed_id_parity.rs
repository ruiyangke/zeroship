//! Wire parity between the platform and migration typed-id codecs.

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
        let core = zeroship_id::typed_id::uuid_to_base36(&id);
        let engine = zeroship_migrate_ir::id::uuid_to_base36(&id);
        assert_eq!(
            core, engine,
            "typed-id encoding diverged between zeroship-id and zeroship-migrate-ir for {id}"
        );
    }
}

#[test]
fn core_and_engine_decode_each_others_ids() {
    // Cross-decode, not just same-side round-trip: a shared bug in one
    // direction would still let each side round-trip its own output.
    for id in vectors() {
        let core = zeroship_id::typed_id::uuid_to_base36(&id);
        let engine = zeroship_migrate_ir::id::uuid_to_base36(&id);

        let core_reads_engine = zeroship_id::typed_id::base36_to_uuid(&engine)
            .expect("core must parse an engine-encoded id");
        let engine_reads_core = zeroship_migrate_ir::id::base36_to_uuid(&core)
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
    // The low sweep is the part that does the work: n in 0..36 encodes to 24
    // zeros plus the n-th alphabet character, so the sorted-ness assertion
    // below reads the alphabet's order directly, one character per value. A
    // sweep of only high-bit values (`n << 80`) never lands on most digits and
    // silently misses a reordering -- verified by swapping the last two
    // characters in BOTH copies, which that sweep alone reports as clean.
    let mut values: Vec<u128> = (0u128..36).chain((0u128..512).map(|n| n << 80)).collect();
    values.sort_unstable();
    values.dedup();
    let ascending: Vec<Uuid> = values
        .into_iter()
        .map(|n| Uuid::from_bytes(n.to_be_bytes()))
        .collect();

    let core: Vec<String> = ascending
        .iter()
        .map(zeroship_id::typed_id::uuid_to_base36)
        .collect();
    let engine: Vec<String> = ascending
        .iter()
        .map(zeroship_migrate_ir::id::uuid_to_base36)
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
