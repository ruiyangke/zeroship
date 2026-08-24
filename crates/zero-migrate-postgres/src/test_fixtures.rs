//! TEST-ONLY charter fixtures for this crate's unit tests.
//!
//! The engine's `zero_migrate::test_fixtures::{no_inject, operator_with_data_security}`
//! are `pub(crate)`, and no visibility widening can make a `pub(crate)` reachable
//! across a crate boundary — so when the PostgreSQL execution half moved here, the
//! tests that came with it needed a sibling. This is it, and it is the same shape
//! `zero-migrate-sqlite`'s and `zero-migrate-mysql`'s `src/test_fixtures.rs` have.
//!
//! What it does NOT do is restate the composition algebra. The real one is
//! `zero_migrate_ir::policy_registry`, so this builds the charter TOML and hands it
//! straight there. One composition, and a change to the algebra cannot leave a
//! vendor's tests asserting against an older one.
//!
//! The charter TEXT is a copy of the engine's, deliberately: these fixtures exist to
//! keep the moved tests asserting against the same policy they asserted against
//! before the move, so a divergence here would be a silent change of premise.
//! `toml::Value` does the schema-name escaping for the same reason — the same
//! escaping, not merely equivalent escaping.

use zero_migrate_ir::policy::DestructiveOps;
use zero_migrate_ir::policy_registry::effective_policy_from_charter_toml;
use zero_migrate_policy::EffectivePolicy;

pub(crate) fn no_inject(schema: &str) -> EffectivePolicy {
    no_inject_with_data_security(schema, false, DestructiveOps::Allow)
}

pub(crate) fn no_inject_with_data_security(
    schema: &str,
    require_rls: bool,
    destructive_ops: DestructiveOps,
) -> EffectivePolicy {
    let schema = toml::Value::String(schema.to_string());
    let destructive_rule = match destructive_ops {
        DestructiveOps::Allow => {
            r#"
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
        }
        DestructiveOps::Warn => {
            r#"
[[grant]]
key = "safety.destructive_ops"
value = "warn"
scope = "all"
"#
        }
        DestructiveOps::Forbid => "",
    };
    let require_rule = if require_rls {
        r#"
[[require]]
key = "safety.require_rls"
value = true
scope = "all"
"#
    } else {
        ""
    };
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
{destructive_rule}{require_rule}
"#
    );
    effective_policy_from_charter_toml(&toml).expect("explicit no-inject test charter composes")
}

pub(crate) fn operator_with_data_security(
    schemas: &[&str],
    extensions: &[&str],
    require_rls: bool,
    destructive_ops: DestructiveOps,
) -> EffectivePolicy {
    let schemas = toml::Value::Array(
        schemas
            .iter()
            .map(|schema| toml::Value::String((*schema).to_string()))
            .collect(),
    );
    let scope = if schemas.as_array().is_some_and(Vec::is_empty) {
        "\"all\"".to_string()
    } else {
        format!("{{ include = {schemas} }}")
    };
    let extension_rule = if extensions.is_empty() {
        String::new()
    } else {
        let extensions = toml::Value::Array(
            extensions
                .iter()
                .map(|extension| toml::Value::String((*extension).to_string()))
                .collect(),
        );
        format!(
            r#"
[[grant]]
key = "code.extension"
value = {extensions}
scope = "all"
"#
        )
    };
    let destructive_rule = match destructive_ops {
        DestructiveOps::Allow => {
            r#"
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
        }
        DestructiveOps::Warn => {
            r#"
[[grant]]
key = "safety.destructive_ops"
value = "warn"
scope = "all"
"#
        }
        DestructiveOps::Forbid => "",
    };
    let require_rule = if require_rls {
        r#"
[[require]]
key = "safety.require_rls"
value = true
scope = "all"
"#
    } else {
        ""
    };
    let toml = format!(
        r#"policy_version = 1

[[grant]]
key = "access.role"
value = true
scope = "all"

[[grant]]
key = "access.grant"
value = true
scope = "all"

[[grant]]
key = "schema.create_schema"
value = true
scope = "all"

[[grant]]
key = "access.policy"
value = true
scope = "all"

[[grant]]
key = "access.rls"
value = true
scope = "all"

[[grant]]
key = "schema.partition"
value = true
scope = "all"

[[grant]]
key = "code.function"
value = true
scope = "all"

[[grant]]
key = "sql.raw"
value = true
scope = "all"

[[grant]]
key = "sql.raw_view_body"
value = true
scope = "all"

[[grant]]
key = "code.materialized_view"
value = true
scope = "all"

[[grant]]
key = "code.trigger"
value = true
scope = "all"

[[grant]]
key = "schema.cross_schema"
value = true
scope = {scope}

[[grant]]
key = "schema.create_table"
value = true
scope = {scope}

[[grant]]
key = "schema.rename"
value = true
scope = {scope}
{extension_rule}{destructive_rule}{require_rule}
"#
    );
    effective_policy_from_charter_toml(&toml).expect("explicit operator test charter composes")
}
