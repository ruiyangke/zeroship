use zeroship_migrate::{effective_policy_from_charter_toml, EffectivePolicy};

pub fn no_inject_charter_toml(schema: &str) -> String {
    let schema = serde_json::to_string(schema).expect("test schema serializes");
    format!(
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
    )
}

pub fn no_inject(schema: &str) -> EffectivePolicy {
    effective_policy_from_charter_toml(&no_inject_charter_toml(schema))
        .expect("explicit no-inject test charter composes")
}

/// Assignments required by fixtures that enable soft deletion and versioning.
pub fn lifecycle_charter_toml(schema: &str) -> String {
    let mut charter = no_inject_charter_toml(schema);
    charter.push_str(r#"

[[inject]]
scope = "all"
mandatory = true
columns = [
  { name = "removed", type = "timestamptz", nullable = true, assign = { by = "now", on = "delete" } },
  { name = "revision", type = "integer", nullable = false, default = "1", assign = { by = "increment(1)", on = "write" } },
]
"#);
    charter
}
