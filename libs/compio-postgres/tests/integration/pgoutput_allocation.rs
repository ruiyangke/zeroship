//! A hostile pgoutput count must not become a hostile ALLOCATION.
//!
//! `pgoutput::decode` sizes vectors from counts the peer chooses -- the
//! relation-id list in `Truncate` most starkly. Each is clamped against the
//! bytes actually remaining (`nrelations.min(cur.len() / 4)`), so a frame
//! claiming `u32::MAX` relations reserves for what the frame can really hold.
//!
//! WHY THIS FILE EXISTS. `replication.rs`'s own
//! `pgoutput_decode_rejects_a_truncate_count_larger_than_the_frame` asserts
//! that such a frame returns `UnexpectedEof` -- and it does, WITH OR WITHOUT
//! the clamp, because the decode runs out of bytes either way. Measured
//! 2026-08-23: deleting `.min(cur.len() / 4)` leaves that test green. It pins
//! the error and is blind to the allocation, which is the only thing the clamp
//! changes. An unclamped `u32::MAX` count reserves about 17 GB; under Linux
//! overcommit that SUCCEEDS without touching a page, so no assertion about the
//! decode's RESULT can see it.
//!
//! HOW IT IS OBSERVED, and why not with an allocator. A counting
//! `#[global_allocator]` is the obvious instrument and is unavailable: the
//! workspace sets `unsafe_code = "deny"` (root `Cargo.toml`), and implementing
//! `GlobalAlloc` requires `unsafe impl`. Exempting a test from a workspace-wide
//! safety policy to measure something is the wrong trade, so this reads
//! `VmPeak` from `/proc/self/status` instead -- the kernel's own high-water
//! mark for virtual size, which a 17 GB reservation moves and a clamped one
//! does not. Linux-only, hence the `cfg`.

#![cfg(target_os = "linux")]

use compio_postgres::replication::pgoutput;

/// The process's peak virtual size in kB, from the kernel.
///
/// `VmPeak` is a high-water mark and never decreases, so only the DELTA across
/// an operation is meaningful -- an absolute value would carry every earlier
/// allocation in the binary.
fn vm_peak_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status")
        .expect("/proc/self/status is readable on Linux");
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmPeak:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
        .expect("VmPeak is present in /proc/self/status")
}

/// A `Truncate` claiming `u32::MAX` relations reserves for the frame, not the
/// claim -- and the instrument that says so can see a real reservation.
///
/// BOTH HALVES LIVE IN ONE TEST ON PURPOSE, and getting this wrong is
/// instructive. They were two `#[test]` functions first, and under the
/// clamp-removed mutation BOTH failed: `VmPeak` is a process-wide HIGH-WATER
/// MARK that never decreases, so whichever test ran first consumed the
/// headroom and left the other measuring a delta of zero. Two tests sharing a
/// monotonic process-global cannot both measure deltas of it, and which one is
/// blinded depends on an ordering neither controls.
///
/// Sequencing them inside a single function makes the order explicit: the
/// hostile decode is measured against a clean baseline FIRST, then the probe
/// establishes that the same instrument registers a multi-gigabyte
/// reservation. Without the second half the first would pass equally if
/// `vm_peak_kb` never moved for any reason -- an instrument that cannot
/// register a positive reads exactly like a well-behaved decoder.
/// THIS SURVIVES SHARING A PROCESS WITH THE WHOLE SUITE, which it now does:
/// these files became modules of one test binary on 2026-08-26, so `VmPeak`
/// arrives here already raised by 74 other modules. That only moves the
/// baseline UP, which shrinks the first delta - the direction that would let
/// the decode assertion pass without proving anything. The probe below is what
/// makes that safe: if earlier work had consumed the headroom, its own delta
/// would be zero and the test FAILS rather than passing empty. So a pass here
/// still means the instrument was live when the verdict was taken.
#[test]
fn a_hostile_truncate_count_does_not_reserve_for_the_claim() {
    // tag, count = u32::MAX, options = 0, and no relation ids at all.
    let frame = vec![b'T', 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

    let before = vm_peak_kb();
    // The decode still fails -- that is what the existing test pins. What
    // matters here is what it reserved on the way.
    assert!(
        pgoutput::decode(&frame).is_err(),
        "a truncate with no ids must not decode"
    );
    let decode_growth_kb = vm_peak_kb().saturating_sub(before);

    // Unclamped this reserves u32::MAX * 4 bytes, about 16.8 million kB. The
    // ceiling is deliberately loose -- this distinguishes gigabytes from
    // nothing, it does not count bytes.
    assert!(
        decode_growth_kb < 100_000,
        "decoding a 6-byte frame grew peak virtual size by {decode_growth_kb} \
         kB; the peer's count reached the allocator unclamped"
    );

    // Now prove the instrument is live, reserving roughly what an unclamped
    // count would have.
    let before_probe = vm_peak_kb();
    {
        let big: Vec<u32> = Vec::with_capacity(u32::MAX as usize);
        assert_eq!(big.capacity(), u32::MAX as usize);
        std::hint::black_box(&big);
    }
    let probe_growth_kb = vm_peak_kb().saturating_sub(before_probe);
    assert!(
        probe_growth_kb > 1_000_000,
        "the probe failed to observe a multi-gigabyte reservation, so the \
         verdict above means nothing: {probe_growth_kb} kB"
    );
}

/// The clamp must not break the honest case.
#[test]
fn a_well_formed_truncate_still_decodes() {
    let mut frame = vec![b'T'];
    frame.extend_from_slice(&2u32.to_be_bytes()); // two relations
    frame.push(0); // options
    frame.extend_from_slice(&16u32.to_be_bytes());
    frame.extend_from_slice(&32u32.to_be_bytes());

    match pgoutput::decode(&frame).expect("a well-formed truncate decodes") {
        pgoutput::PgOutputMessage::Truncate { relation_ids, .. } => {
            assert_eq!(relation_ids, vec![16, 32]);
        }
        other => panic!("expected a Truncate, got {other:?}"),
    }
}
