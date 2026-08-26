//! A vendor's capability row has ONE home, and its two readers must reach it.
//!
//! # The two readers
//!
//! `BackendVendor::descriptor` is the row the registry composed. `DmlRenderer::descriptor()`
//! is the row the renderer answers from — and it is the one that decides capability
//! questions at lower time, because `require_capability_for` asks `self.backend`, a
//! `&dyn DmlRenderer`, not the composed vendor.
//!
//! Nothing makes those the same row. Each shipping vendor happens to return the same
//! `&'static` from both, by convention, and that convention is what this file turns
//! into a check.
//!
//! # Why it is worth a test rather than a comment
//!
//! It was found the way these things usually are: as a confusing failure with the wrong
//! shape. An agent building an unrelated fixture borrowed one backend's renderer for a
//! stand-in vendor and got `TableRebuildUnavailable` — a refusal about table rebuilds,
//! for a capability question about something else entirely. The borrowed renderer had
//! brought its own capability row along, and the gate answered about the wrong backend.
//!
//! That is the whole failure mode, and it is quiet: a capability is the engine asking
//! "may this op run against THIS target". If the answer can arrive from a row the
//! registry did not compose, an op can be admitted on a backend that never declared it,
//! or refused on one that did. Neither shows up as a type error.
//!
//! This is a LATENT hole, stated plainly: at the time of writing all three shipping
//! vendors return the same static from both readers, so nothing is wrong today. The
//! test exists so that stays true by construction rather than by everyone remembering.
//!
//! # Shape
//!
//! Pointer identity, not equality. `BackendDescriptor` could grow `PartialEq` and two
//! structurally-equal rows would then pass while still being two objects that can drift
//! apart on the next edit. The claim worth making is that there is ONE row.
//!
//! Sibling in spirit to `the_registry_travels_as_a_value::the_two_compositions_list_the_same_vendors`,
//! which guards the other duplication the workspace grew: two lists of vendors rather
//! than two rows per vendor.

use std::ptr;

/// Every shipping vendor answers both descriptor readers with the same row.
#[test]
fn a_vendors_two_descriptor_readers_reach_one_row() {
    let vendors = zero_migrate::shipping_vendors();

    // A census over a DISCOVERED set fails OPEN: an empty slice satisfies every
    // per-vendor claim below without checking anything. The floor is the shipping
    // count, so losing a backend is a red here too.
    assert!(
        vendors.len() >= 3,
        "the shipping composition holds {} vendors; this file's per-vendor claims are \
         vacuous below the shipping count, so a shrunken registry must fail here rather \
         than pass quietly",
        vendors.len()
    );

    for vendor in vendors.as_slice() {
        let composed = vendor.descriptor;
        let answered = vendor.dml.descriptor();

        assert!(
            ptr::eq(composed, answered),
            "`{}` answers two different capability rows: the registry composed one at \
             `BackendVendor::descriptor` and its `DmlRenderer::descriptor()` returns \
             another. Lower-time capability gates read the RENDERER's row, so the two \
             can disagree about what this backend supports and nothing else would say \
             so. Return the same `&'static` from both.",
            composed.id.as_str(),
        );
    }
}

/// The check above can distinguish one row from two.
///
/// Without this, `ptr::eq` over a set whose members all happen to be the same object
/// would pass just as well if the assertion were inverted, or if `as_slice` yielded
/// nothing. Here are two rows that are structurally identical and still two objects:
/// the positive control proves the instrument sees the difference it claims to see.
#[test]
fn pointer_identity_separates_one_row_from_two_equal_ones() {
    let vendors = zero_migrate::shipping_vendors();
    let first = vendors
        .as_slice()
        .first()
        .expect("the shipping composition is not empty");

    let same = first.descriptor;
    assert!(
        ptr::eq(first.descriptor, same),
        "the control's own premise failed: one row is not identical to itself"
    );

    let copied = first.descriptor.clone();
    assert!(
        !ptr::eq(first.descriptor, ptr::from_ref(&copied)),
        "a CLONE of the row compared identical to the original, so `ptr::eq` is not \
         distinguishing objects here and the check above proves nothing"
    );
}
