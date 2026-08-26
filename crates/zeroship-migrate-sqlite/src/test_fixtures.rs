//! TEST-ONLY charter fixtures for this crate's unit tests.
//!
//! The engine's `zeroship_migrate::test_fixtures::no_inject` is `pub(crate)`, and no
//! visibility widening can make a `pub(crate)` reachable across a crate boundary -
//! so when the SQLite execution half moved here, its four project-lock tests needed
//! a sibling. This is it, and it is the same shape `zero-migrate-mysql`'s
//! `src/test_fixtures.rs` already has.
//!
//! What it does NOT do is restate the composition algebra. The real one is
//! `zeroship_migrate_ir::policy_registry`, so this builds the charter TOML and hands it
//! straight there. One composition, and a change to the algebra cannot leave a
//! vendor's tests asserting against an older one.
//!
//! The charter TEXT is a copy of the engine's, deliberately: these fixtures exist to
//! keep the moved tests asserting against the same policy they asserted against
//! before the move, so a divergence here would be a silent change of premise.
//! `toml::Value` does the schema-name escaping for the same reason - the same
//! escaping, not merely equivalent escaping.

use zeroship_migrate_ir::policy_registry::effective_policy_from_charter_toml;
use zeroship_migrate_policy::EffectivePolicy;

/// A charter that grants this schema the table/rename/cross-schema verbs and injects
/// nothing, so an authored table reaches the backend with exactly its own columns.
pub(crate) fn no_inject(schema: &str) -> EffectivePolicy {
    let schema = toml::Value::String(schema.to_string());
    let toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = [{schema}] }}

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema}] }}

[[grant]]
key = "schema.rename"
value = true
scope = {{ include = [{schema}] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
    );
    effective_policy_from_charter_toml(&toml)
        .unwrap_or_else(|error| panic!("the explicit no-inject test charter composes: {error:?}"))
}
