//! One vendor, two doors: the closed-enum bridge and the registry must hand back the
//! SAME capability row — every field of it, not just the id.
//!
//! # The seam, stated once
//!
//! `SqlDialect::descriptor()` lives in `zero-migrate-ir/src/backend.rs`. `-ir` is the
//! BOTTOM of the crate graph: every vendor crate depends on it and it may name none of
//! them. So the closed enum's bridge cannot resolve `zero_migrate_postgres`'s row by
//! reaching up for it — the row has to already be down there, and satisfying a FOURTH
//! variant would mean copying a fourth vendor's answers DOWN into `-ir`.
//!
//! Meanwhile the registry resolves the other way round. `zero_migrate::shipping_backends()`
//! is derived from `render::backends::VENDORS`, i.e. from the vendor crates ACTUALLY
//! COMPILED IN, each of which names its own row in its own `BackendVendor` literal.
//!
//! Two doors, opposite directions, same question. Nothing before this file made them
//! answer it the same way.
//!
//! # What was already checked, and the gap it left
//!
//! Two existing tests look like they cover this and do not:
//!
//!   - `render::backends`'s `every_vendor_agrees_with_its_own_descriptor` compares
//!     `v.schema.dialect().id()` against `v.descriptor.id`. That is a RENDERER-to-id
//!     check, and `id` is the one field that cannot carry a capability disagreement.
//!     A vendor whose row answered NO to a capability its renderer emits passes it.
//!   - `zero-migrate-ir`'s `the_shipping_registry_builds` does compare whole
//!     descriptors — but both of its sides live in `-ir`: `BackendRegistry::shipping()`
//!     is built from `-ir`'s own `SHIPPING_DESCRIPTORS` constant, and the bridge is
//!     `-ir`'s own match. It cannot see across the crate boundary, because `-ir` cannot
//!     name a vendor crate. That is precisely the boundary a copy-down straddles: point
//!     the bridge arm AND `SHIPPING_DESCRIPTORS` at a copy and that test stays green
//!     while the compiled-in vendor keeps the original.
//!
//! This file is an integration test of `zero-migrate`, which depends on all three
//! vendor crates, so it is the lowest place in the graph where BOTH doors can be
//! opened at once.
//!
//! # The measured truth on the unmodified tree, taken BEFORE this assertion was written
//!
//! There is no drift to ratchet. All three vendors agree across all three routes, on
//! every field. A probe run at `6dfcd830` printed, for each of Postgres / Sqlite /
//! Mysql: `eq(bridge,vendor)=true eq(bridge,ir)=true`, capability diff `[]`.
//!
//! Two things about that measurement are worth writing down, because one of them is
//! the reason this test is not vacuous and the other is the reason it currently cannot
//! be provoked in the obvious way:
//!
//!   1. THE COPY IS PHYSICAL BUT THE SOURCE IS SINGLE. The same probe printed three
//!      DIFFERENT addresses per vendor (`ptr_eq(bridge,vendor)=false`,
//!      `ptr_eq(bridge,ir)=false`). `POSTGRES_DESCRIPTOR` and friends are `const`, not
//!      `static`, so every `&CONST` site materialises its own promoted copy in its own
//!      crate's rodata. The equality below is therefore a real byte-for-byte comparison
//!      of three separately-materialised rows, not a pointer identity dressed up as one.
//!   2. THE VENDORS DO NOT OWN THEIR ROWS TODAY. Each vendor crate's `BackendVendor`
//!      literal writes `descriptor: &zero_migrate_ir::backend::<X>_DESCRIPTOR` — it
//!      REFERENCES `-ir`'s constant rather than declaring its own. So today there is
//!      exactly one DECLARATION per vendor behind all three routes. Flip a bit in
//!      `MYSQL_CAPABILITIES` and all three routes move together and this test stays
//!      green — which is not a hole in the test, it is the tree correctly having no
//!      copy to disagree with. This was verified by inversion rather than assumed; see
//!      the note on the RED demonstration below.
//!
//! What this test pins, then, is that the single declaration STAYS single. The moment a
//! second one appears — a fourth backend forcing a copy-down, a vendor crate promoting
//! its row to its own constant, an `-ir` bumped out from under a vendor rlib — the two
//! doors start answering from different rows and this fires, naming the vendor and the
//! exact capability.
//!
//! # How the RED was demonstrated
//!
//! By materialising the copy the paragraph above describes: a second descriptor in
//! `-ir` differing from `MYSQL_DESCRIPTOR` by one bit (`TriggerBody`), with the bridge's
//! `Mysql` arm pointed at it and `SHIPPING_DESCRIPTORS` left alone. The test failed
//! naming `mysql` and `TriggerBody`. The floor was demonstrated separately by emptying
//! the iteration. Both edits were reverted; no production code ships from this commit.
//!
//! # Defences carried
//!
//! Per `core_does_not_spell_a_vendors_bytes.rs`, which carries them because each
//! corresponding failure actually happened in this tree:
//!
//!   - A CENSUS FLOOR. This iterates a DISCOVERED set — whatever the registry holds —
//!     and a scan over a discovered set FAILS OPEN. Register nothing and the loop
//!     visits nothing, finds no disagreement, and reports clean. The floor below is
//!     what makes an empty registry a RED.
//!   - THE FOUND SET READ BACK. A bare count passes on N-1 members plus a coincidence.
//!     The ids are compared as an ordered list, so a vendor swapped for another cannot
//!     hide behind an unchanged length.
//!   - BOTH DIRECTIONS. A vendor with no enum variant is unreachable through the
//!     bridge; an enum variant with no vendor is a descriptor for a backend this build
//!     does not ship. Either is a red, so neither can be reached by deleting the other
//!     side.

use zero_migrate::{
    shipping_backends, BackendDescriptor, BackendRegistry, Capability, DialectId, SqlDialect,
};

/// The floor: fewer registered vendors than this and the iteration below is not
/// measuring the tree, it is measuring nothing.
///
/// Raise it when a backend is ADDED. If one is genuinely removed, lower it deliberately
/// in the commit that removes it and say so — never to get green.
const REGISTERED_VENDOR_FLOOR: usize = 3;

/// The found set, read back by NAME and in registration order.
///
/// The floor above defends against discovering nothing; this defends against
/// discovering the wrong three. A count alone is satisfied by two shipping vendors plus
/// a test double, and by a rename that quietly drops a backend out of the comparison.
const EXPECTED_VENDOR_IDS: &[&str] = &["postgres", "sqlite", "mysql"];

/// Every variant of the closed enum, which is the bridge's whole domain.
///
/// Kept honest by [`variant_index`] rather than by a comment: that function's `match`
/// is exhaustive over `SqlDialect`, so a fourth variant fails to COMPILE here, and the
/// assertion in [`the_variant_list_is_the_whole_enum`] then refuses to pass until the
/// new variant is in this list too. A list that could silently fall behind the enum
/// would shrink the second direction of the check without shrinking its green.
const ALL_SQL_DIALECTS: &[SqlDialect] =
    &[SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql];

/// This variant's position in [`ALL_SQL_DIALECTS`].
///
/// The `match` is deliberately exhaustive and deliberately hand-indexed: adding a
/// `SqlDialect` variant is a compile error in this file, and the only way to fix it is
/// to give the new variant an index, which is only correct if it was also appended to
/// the list.
fn variant_index(dialect: SqlDialect) -> usize {
    match dialect {
        SqlDialect::Postgres => 0,
        SqlDialect::Sqlite => 1,
        SqlDialect::Mysql => 2,
    }
}

/// The enum variant that answers for `id`, if the closed enum has one.
fn bridge_variant(id: DialectId) -> Option<SqlDialect> {
    ALL_SQL_DIALECTS.iter().copied().find(|d| d.id() == id)
}

/// Every capability the two rows disagree about, by name, with which side said yes.
///
/// Naming the CAPABILITY is the point. `assert_eq!` on two `BackendDescriptor`s prints
/// two opaque `CapabilitySet(u64)` bitmasks and leaves the reader to diff them by hand
/// — and the failure this guards against is a SINGLE bit, which is exactly the case a
/// hex diff is worst at.
fn capability_disagreements(
    route: &str,
    bridge: &BackendDescriptor,
    registry: &BackendDescriptor,
) -> Vec<String> {
    Capability::ALL
        .iter()
        .filter(|cap| bridge.capabilities.contains(**cap) != registry.capabilities.contains(**cap))
        .map(|cap| {
            let (yes, no) = if bridge.capabilities.contains(*cap) {
                ("the enum bridge", route)
            } else {
                (route, "the enum bridge")
            };
            format!("{cap:?}: {yes} answers YES, {no} answers NO")
        })
        .collect()
}

/// A full field-by-field account of one vendor's disagreement.
fn describe(
    route: &str,
    id: &str,
    bridge: &BackendDescriptor,
    registry: &BackendDescriptor,
) -> String {
    let mut lines = vec![format!(
        "vendor `{id}`: the enum bridge and {route} disagree"
    )];
    if bridge.id != registry.id {
        lines.push(format!(
            "  id: bridge={:?} vs {route}={:?}",
            bridge.id.as_str(),
            registry.id.as_str()
        ));
    }
    if bridge.display_name != registry.display_name {
        lines.push(format!(
            "  display_name: bridge={:?} vs {route}={:?}",
            bridge.display_name, registry.display_name
        ));
    }
    for line in capability_disagreements(route, bridge, registry) {
        lines.push(format!("  capability {line}"));
    }
    if bridge.limits != registry.limits {
        lines.push(format!(
            "  limits: bridge={:?} vs {route}={:?}",
            bridge.limits, registry.limits
        ));
    }
    if lines.len() == 1 {
        // `==` said no but no field did. Report it rather than printing a heading with
        // nothing under it: a comparison that disagrees about disagreement is itself the
        // finding, and silence here would look like a passing vendor.
        lines.push(
            "  (no field differs, yet the descriptors compare unequal — BackendDescriptor \
             has grown a field this test does not print)"
                .to_string(),
        );
    }
    lines.join("\n")
}

/// [`ALL_SQL_DIALECTS`] is the whole enum and holds each variant exactly once.
///
/// Split out so its failure is unambiguous. Folded into the main test it would read as
/// a bridge/registry disagreement, which is a different defect with a different fix.
#[test]
fn the_variant_list_is_the_whole_enum() {
    for (index, dialect) in ALL_SQL_DIALECTS.iter().enumerate() {
        assert_eq!(
            variant_index(*dialect),
            index,
            "{dialect:?} sits at index {index} of ALL_SQL_DIALECTS but `variant_index` \
             files it elsewhere. The `match` in `variant_index` is exhaustive, so a new \
             variant breaks the build there; this is the other half — the new variant \
             must also be APPENDED to ALL_SQL_DIALECTS, or the bridge/registry check \
             silently stops covering it."
        );
    }
    assert_eq!(
        ALL_SQL_DIALECTS.len(),
        REGISTERED_VENDOR_FLOOR,
        "the closed enum has {} variant(s) but at least {REGISTERED_VENDOR_FLOOR} \
         backend(s) are expected to ship. These two move together: a fourth backend is a \
         fourth variant AND a fourth vendor, and if it is genuinely only one of those, \
         say which here.",
        ALL_SQL_DIALECTS.len()
    );
}

/// THE ASSERTION THIS FILE EXISTS FOR: for every registered vendor, the descriptor
/// reached through the closed-enum bridge equals the one reached through the registry —
/// capabilities, limits, display name and id, not just id.
///
/// Both registries are checked against the bridge, and they are different questions.
/// `shipping_backends()` is derived from the vendor crates COMPILED IN, so it is the
/// one a copy-down would leave behind. `BackendRegistry::shipping()` is `-ir`'s own
/// list, so a bridge arm repointed without touching that list is caught by it. A
/// copy-down that updates one of the two and not the other is caught by whichever it
/// missed.
#[test]
fn the_enum_bridge_and_the_registry_resolve_to_one_descriptor() {
    let vendors: BackendRegistry = shipping_backends();
    let ir_shipping: BackendRegistry = BackendRegistry::shipping();

    // FLOOR. This iterates a DISCOVERED set and therefore fails OPEN: an empty registry
    // visits no vendor, finds no disagreement, and reports clean.
    assert!(
        vendors.len() >= REGISTERED_VENDOR_FLOOR,
        "the vendor registry holds {} backend(s), expected at least \
         {REGISTERED_VENDOR_FLOOR}.\n\
         \n\
         Everything below iterates this registry, so a narrowed or broken registration \
         makes the whole check iterate NOTHING and pass. Raise this floor when a backend \
         is ADDED; if one was genuinely REMOVED, lower it deliberately and say which — \
         do not lower it to get green.",
        vendors.len()
    );

    // FOUND-SET READBACK. Ordered and by name, so N-1 members plus a coincidence, or a
    // rename, cannot pass behind an unchanged count.
    let found: Vec<&str> = vendors.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(
        found, EXPECTED_VENDOR_IDS,
        "the vendor registry holds a different set than this file expects.\n\
         \n\
         found:    {found:?}\n\
         expected: {EXPECTED_VENDOR_IDS:?}\n\
         \n\
         The count alone is not the property — a backend swapped for another keeps it. \
         If a backend was added or removed, update this list and the floor in the same \
         commit."
    );
    let ir_found: Vec<&str> = ir_shipping.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(
        ir_found, found,
        "`zero-migrate-ir`'s SHIPPING_DESCRIPTORS and the compiled-in vendor set name \
         different backends.\n\
         \n\
         `-ir` list:   {ir_found:?}\n\
         vendor crates: {found:?}\n\
         \n\
         `SHIPPING_DESCRIPTORS` is a constant in the leaf crate and would still name \
         three backends in a build that linked none of them. When the two lists diverge \
         the leaf crate is describing a build it is not in."
    );

    let mut checked = 0_usize;
    let mut disagreements: Vec<String> = Vec::new();

    for registered in vendors.iter() {
        let id = registered.id.as_str();

        let Some(dialect) = bridge_variant(registered.id) else {
            disagreements.push(format!(
                "vendor `{id}` is registered but no `SqlDialect` variant answers for it, \
                 so it is unreachable through the enum bridge entirely. Engine code still \
                 holding a `SqlDialect` cannot ask this backend anything."
            ));
            continue;
        };

        let bridge = dialect.descriptor();
        if bridge != registered {
            disagreements.push(describe("the vendor registry", id, bridge, registered));
        }
        if let Some(from_ir) = ir_shipping.get(registered.id) {
            if bridge != from_ir {
                disagreements.push(describe("`-ir`'s shipping registry", id, bridge, from_ir));
            }
        } else {
            disagreements.push(format!(
                "vendor `{id}` ships in this build but `zero-migrate-ir`'s \
                 SHIPPING_DESCRIPTORS does not list it."
            ));
        }

        checked += 1;
    }

    // The other direction: a variant the registry does not answer for. Checked here
    // rather than in a second test because it is the same defect seen from the far
    // side — one door open, the other shut.
    for dialect in ALL_SQL_DIALECTS {
        if vendors.get(dialect.id()).is_none() {
            disagreements.push(format!(
                "`SqlDialect::{dialect:?}` hands out a descriptor through the bridge but \
                 no vendor is registered under `{}`, so the bridge is describing a \
                 backend this build does not ship.",
                dialect.id().as_str()
            ));
        }
    }

    assert!(
        disagreements.is_empty(),
        "the closed-enum bridge and the registry do not resolve to one descriptor:\n\n\
         {}\n\
         \n\
         `SqlDialect::descriptor()` is in `zero-migrate-ir`, BELOW every vendor crate, so \
         its answers cannot be resolved from a vendor and must be spelled in `-ir` \
         itself. The registry's answers come from the vendor crates compiled in. When \
         those two spellings stop being the same declaration, capability questions get \
         DIFFERENT ANSWERS DEPENDING ON WHICH DOOR THE CALLER USED, inside one process, \
         with no crash and no wrong-looking SQL for any vendor that happens to agree.\n\
         \n\
         Fix the ROW, not this test: make the vendor's descriptor and `-ir`'s the same \
         declaration again. Two rows that are merely kept equal by hand is the state \
         this file exists to make impossible.",
        disagreements.join("\n\n")
    );

    // The loop ran over every member it discovered. Without this, a `continue` added
    // above could skip vendors while the floor and the readback both stay green.
    assert_eq!(
        checked,
        vendors.len(),
        "the comparison ran for {checked} of {} registered vendor(s). The floor and the \
         found-set readback both check the SET; this checks that the LOOP consumed it.",
        vendors.len()
    );
}
