#![allow(dead_code)] // Not every integration test binary uses every fixture.

use zero_migrate::{effective_policy_from_charter_toml, EffectivePolicy};

pub const CONFINED_CHARTER_TOML: &str = r#"policy_version = 1

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

pub fn confined_charter() -> EffectivePolicy {
    effective_policy_from_charter_toml(CONFINED_CHARTER_TOML)
        .expect("explicit confined test charter composes")
}

fn no_inject_charter_toml(schema: &str) -> String {
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
