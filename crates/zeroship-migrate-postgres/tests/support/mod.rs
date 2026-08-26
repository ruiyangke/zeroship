#![allow(dead_code)]

use zeroship_migrate_policy::EffectivePolicy;

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
}

pub fn no_inject(schema: &str) -> EffectivePolicy {
    let schema = serde_json::to_string(schema).expect("schema serializes as a TOML string");
    let charter = format!(
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
    effective_policy_from_charter_toml(&charter)
}

// This used to be a SECOND implementation of charter composition, kept here only
// because the real one lived in the engine and this crate sits below the engine.
// The real one moved down to `zeroship_migrate_ir::policy_registry`, so this delegates
// to it: one composition, one grant-only-draft extractor, no drift between what a
// vendor's tests compose and what production composes.
pub fn effective_policy_from_charter_toml(charter_toml: &str) -> EffectivePolicy {
    zeroship_migrate_ir::policy_registry::effective_policy_from_charter_toml(charter_toml)
        .expect("test policy composes")
}
