/**
 * The zeroship CONFINED schema-emit ceiling — a bundled, client-side type-gen
 * constant the gen-types orchestrator threads into `genArtifacts` as
 * `policyCeilingToml`.
 *
 * WHY this lives here (and is a constant, not a file read): the `zero-migrate`
 * engine is deliberately PRESET-FREE — it bakes in no confined ceiling. The
 * SCHEMA-EMIT injection shape (the seven platform system columns + `["id"]` PK +
 * the three system indexes) is supplied by the CALLER. gen-types runs entirely
 * client-side at build time (record migrations / evaluate schema.ts → `genArtifacts`
 * → `env.db.ts` + `schema.runtime.json`); it never touches a database and never runs
 * the migration guard. So the emit path needs ONLY the `[[inject]]` rule that drives
 * `resolve_create_table_policy` — NOT the destructive-op posture, timeout ceilings, or
 * `core.*` grants the full apply-side `crates/migrated/policies/confined.policy.toml`
 * ceiling carries (those govern the guarded apply, which the emitter never reaches).
 *
 * The injection shape here MUST match the apply-side confined ceiling's `[[inject]]`
 * rule (`crates/migrated/policies/confined.policy.toml`) so the emitted
 * `schema.runtime.json` describes the SAME table shape the migration apply produces.
 * `tests/inject_policy_mirror_gate.sh` enforces that, and does so against all six
 * copies of the rule rather than the two it compared until 2026-08-20.
 *
 * Both gen-types sources (GENERATED envelopes + MANUAL descriptors) pass this SAME
 * ceiling, which is what preserves the byte-identical-by-construction guarantee now
 * that injection is policy-driven rather than a baked-in engine preset.
 */
export const CONFINED_SCHEMA_EMIT_CEILING_TOML = `policy_version = 1

# The mandatory platform system-table shape — the seven system columns + the
# ["id"] primary key + the three system indexes injected into every created table.
# Mirrors the [[inject]] rule of crates/migrated/policies/confined.policy.toml.
[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  # The three NOT NULL columns carry defaults because the data plane does not
  # send them. crud/system_fields_pass.rs omits created_at/updated_at/version
  # from every INSERT by design and relies on the DDL to supply the canonical
  # value; without these the first insert into any migration-created table fails
  # with "null value in column created_at violates not-null constraint".
  # id is deliberately defaultless - it is minted into the INSERT instead.
  # Values match zeroship-schema's system_field_columns so the migration and
  # plugin-db producers emit the same table.
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false, default = "NOW()" },
  { name = "updated_at", type = "timestamptz", nullable = false, default = "NOW()" },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false, default = "1" },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "ix_deleted_at", columns = ["deleted_at"] },
  { name = "ix_updated_at", columns = ["updated_at"] },
  { name = "ix_created_by", columns = ["created_by"] },
]
`;
