//! The postgres backend's integration suites.
//!
//! `Cargo.toml` sets `autotests = false` and registers `tests/main.rs` as the
//! `main` target, so a suite is compiled only once it is declared here. Add
//! `mod <name>;` with the file or its tests never run. The suites reach the
//! shared helpers at the target root as `crate::support::...`.

mod advisories_fire_on_the_real_hazards;
mod attribute_vocabulary_export;
mod confinement_resists_nesting_evasion;
mod denylist_resists_nesting_evasion;
mod extension_drop_is_name_scoped;
mod guard_smoke;
mod namespace_authority;
mod object_scoped_grant_scope;
mod require_rls_scope;
mod table_owner_authority;
