use crate::model::policy::DestructiveOps;
use crate::{effective_policy_from_charter_toml, EffectivePolicy};

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

pub(crate) fn operator_no_inject(schema: &str) -> EffectivePolicy {
    operator_with_data_security(&[schema], &[], false, DestructiveOps::Allow)
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
