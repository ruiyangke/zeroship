/**
 * The zeroship CONFINED schema-emit ceiling — a bundled, client-side type-gen
 * constant the gen-types orchestrator threads into `genArtifacts` as
 * `policyCeilingToml`.
 *
 * WHY this lives here (and is a constant, not a file read): the `@zeroship/migrate`
 * engine is deliberately PRESET-FREE — it bakes in no confined ceiling. The
 * SCHEMA-EMIT injection shape (the seven platform system columns + `["id"]` PK +
 * the three system indexes) is supplied by the CALLER. gen-types runs entirely
 * client-side at build time (record migrations / evaluate schema.ts → `genArtifacts`
 * → `env.db.ts` + `schema.runtime.json`); it never touches a database and never runs
 * the migration guard. So the emit path needs ONLY the `[[inject]]` rule that drives
 * `resolve_create_table_policy` — NOT the destructive-op posture, timeout ceilings, or
 * `core.*` grants the full apply-side `crates/zeroship-migrate-server/policies/confined.policy.toml`
 * ceiling carries (those govern the guarded apply, which the emitter never reaches).
 *
 * The injection shape here is NOT WRITTEN HERE. It is the platform-wide fragment
 * `policies/confined-system-shape.inject.toml`, which the deployed apply-side
 * ceiling (`crates/zeroship-migrate-server/policies/confined.policy.toml`) also takes, so the
 * emitted `schema.runtime.json` cannot describe a different table from the one the
 * migration apply produces. Rust `include_str!`s the fragment; TypeScript gets it
 * through `policies/codegen.mjs`, whose output this file imports.
 *
 * Both gen-types sources (GENERATED envelopes + MANUAL descriptors) pass this SAME
 * ceiling, which is what preserves the byte-identical-by-construction guarantee now
 * that injection is policy-driven rather than a baked-in engine preset.
 */
import { CONFINED_SYSTEM_SHAPE_INJECT_TOML } from "./confined-system-shape.generated.js";

export const CONFINED_SCHEMA_EMIT_CEILING_TOML =
  `policy_version = 1\n\n` + CONFINED_SYSTEM_SHAPE_INJECT_TOML;
