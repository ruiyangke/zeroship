//! TEST-ONLY fixtures for this crate's own unit tests. `lib.rs` declares this module
//! `#[cfg(test)]`, so none of it ships.

use zero_migrate_ir::dialect::DialectId;

use crate::model::policy::DestructiveOps;
use crate::{effective_policy_from_charter_toml, EffectivePolicy};

// THE THREE SHIPPING DIALECT IDS, for core's `#[cfg(test)]` modules.
//
// # Why core re-declares ids it does not own
//
// The ids belong to the vendors: `zero_migrate_postgres::DIALECT` is the workspace's
// one declaration of `"postgres"`, and every consumer OUTSIDE this crate — including
// core's own `tests/` binaries — reads it from there.
//
// Core's `src` cannot. Two censuses forbid any non-comment line under `src/` from
// naming a vendor crate, and neither of them excludes test code:
// `dialect_matrix/core_names_no_vendor_crate.rs` allows `zero_migrate_postgres` only
// in the registry composition, and `core_names_no_vendor_backend_module.rs` allows
// `postgres::` in path position NOWHERE. A `#[cfg(test)] use zero_migrate_postgres::DIALECT`
// in `render/lower.rs` would be a real regression of both, not a technicality — the
// compiled engine would still not link differently, but the rule those censuses hold
// is about what core's source is ALLOWED to know, and twenty-two files knowing it is
// exactly the drift they exist to catch.
//
// So these are re-declared, which is what `DialectId`'s content equality is for:
// `DialectId::new("postgres")` IS the PostgreSQL id, whoever writes it.
// `zero-migrate-ir`'s own tests do the same thing one crate lower, for the same
// reason: a crate that cannot depend on a vendor builds the id it needs.
//
// # Why one module and not a const per test module
//
// Twenty-two `#[cfg(test)]` modules under `src/` name a dialect. Declaring the
// strings in each would put sixty-six copies of three literals in core and give a
// reader twenty-two places to ask whether core is carrying vendor identity. One
// module answers it once, and `core_names_no_vendor_at_all.rs` already resolves a
// parent's `#[cfg(test)] mod` to its file and excludes the whole file from the
// production count.

/// The PostgreSQL id. The declaration that SHIPS is `zero_migrate_postgres::DIALECT`.
pub(crate) const POSTGRES: DialectId = DialectId::new("postgres");
/// The `SQLite` id. The declaration that SHIPS is `zero_migrate_sqlite::DIALECT`.
pub(crate) const SQLITE: DialectId = DialectId::new("sqlite");
/// The `MySQL` id. The declaration that SHIPS is `zero_migrate_mysql::DIALECT`.
pub(crate) const MYSQL: DialectId = DialectId::new("mysql");

pub(crate) const CONFINED_CHARTER_TOML: &str = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }

[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "updated_at", type = "timestamptz", nullable = false },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "ix_deleted_at", columns = ["deleted_at"] },
  { name = "ix_updated_at", columns = ["updated_at"] },
  { name = "ix_created_by", columns = ["created_by"] },
]
"#;

pub(crate) fn confined_charter() -> EffectivePolicy {
    effective_policy_from_charter_toml(CONFINED_CHARTER_TOML)
        .expect("explicit confined test charter composes")
}

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
