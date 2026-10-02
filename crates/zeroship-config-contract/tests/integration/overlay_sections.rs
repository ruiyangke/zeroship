//! The shared typed overlay must admit every setting the binaries declare.
//!
//! Declaring a setting in `zeroship_config` gives it a flag and a
//! `ZEROSHIP_*` name unconditionally, but the overlay tier only works if the
//! typed section in `zeroship-core/src/config/file.rs` has a field for it.
//! Those sections are `#[serde(deny_unknown_fields)]`, so a declared-but-absent
//! setting does not degrade to "ignored in the TOML": an operator who writes it
//! into the shared overlay gets a process that REFUSES TO BOOT.
//!
//! The expectation here is derived, never restated. Every probed path comes from
//! `ConfigSpec::toml_path` on the registries rustc linked into this test binary,
//! which is the same compiled value the resolvers walk. There is no list of
//! setting names to keep in step, because a list is exactly what drifted.
//!
//! Does not cover: whether a supplied overlay value REACHES the setting. That is
//! the resolver's `Toml` read site, and `tests/linked_registry.rs` plus
//! `contract::validate_contract` own it. This file asks only whether the parse
//! survives the key at all.

use zeroship_config_contract::overlay::validate_overlay_paths;
use zeroship_config_contract::registry::platform_specs;

#[test]
fn the_shared_overlay_accepts_every_declared_setting() {
    // Mutation: delete any `Option<T>` field from a section of
    // `zeroship-core/src/config/file.rs` whose canonical path a binary declares.
    // The declaration still compiles, the flag and the environment name still
    // work, and only this check turns red.
    let specs = platform_specs();
    // The instrument, not the subject. `validate_overlay_paths` refuses an empty
    // registry, but saying so here names the cause at the call site rather than
    // leaving a reader to infer it from an anti-vacuity error.
    assert!(
        !specs.is_empty(),
        "the linked registries produced no declarations; every probe below \
         would be vacuous"
    );

    if let Err(errors) = validate_overlay_paths(&specs) {
        let report = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ");
        panic!("declared settings the shared TOML overlay would refuse:\n  {report}");
    }
}
