#![allow(dead_code)]

use zero_migrate_ir::policy_registry::builtin_registry;
use zero_migrate_policy::{admit, EffectivePolicy, LoadContext, PolicyDoc, RootCharter};

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

pub fn effective_policy_from_charter_toml(charter_toml: &str) -> EffectivePolicy {
    let registry = builtin_registry();
    let charter = RootCharter::parse_toml(charter_toml, &registry).expect("test charter parses");
    let draft = PolicyDoc::parse_toml(
        &grant_only_draft_toml(charter_toml),
        &registry,
        LoadContext::NonRootLayer,
    )
    .expect("test grant-only draft parses");
    admit(&charter, &draft, &registry).expect("test policy composes")
}

fn grant_only_draft_toml(charter_toml: &str) -> String {
    let mut out = String::from("policy_version = 1\n");
    let mut in_grant = false;
    for line in charter_toml.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("[[") {
            in_grant = trimmed.starts_with("[[grant]]");
            if in_grant {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if in_grant {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
