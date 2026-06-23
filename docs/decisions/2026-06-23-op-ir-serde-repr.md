# 2026-06-23 — `op.*` IR serde representation: internally-tagged `Op`

**Status:** accepted (immutable once landed).

## Decision

The `op.*` migration IR's operation enum (`crates/zeroship-migrate/src/ir.rs`,
`Op`) is serialized **internally-tagged** via `#[serde(tag = "op")]`, with
`#[serde(rename_all = "camelCase")]` so the discriminant is a stable top-level
`"op"` string key (`{"op":"createTable", …}`). We do **NOT** use serde's
`untagged` or `flatten` representations for the `Op` enum.

## Rationale (§2.1 of the `op.*` DSL design)

- **Internally-tagged keeps the discriminant a stable, named top-level key.**
  `"op"` is a single, predictable JSON pointer that JSON-Schema can express as a
  discriminated union (each `oneOf` branch pins `properties.op.const`), that the
  JS `op.*` builder emits directly, and that the Rust loader + the Wave-E
  exhaustiveness gate can read mechanically (`$defs/Op/oneOf[*].properties.op.const`).
- **`untagged` is ambiguous to deserialize and breaks schemars.** Without a
  discriminant, serde must try every variant in order and accept the first that
  parses — fragile against overlapping shapes, slow, and it produces a
  structurally-ambiguous JSON Schema (no discriminator) that a downstream JS or
  TS generator cannot turn into a tagged union.
- **`flatten` breaks the `JsonSchema` derive.** schemars cannot reliably derive
  a schema for a flattened enum variant; the derive either fails or emits an
  unsound schema. The op-list's whole value as a contract is the JSON Schema we
  emit to `op-ir.schema.json`, so the representation must be schemars-derivable.

`IrConstraint` does use `#[serde(flatten)]` to inline its `IrConstraintKind`
(itself internally-tagged on `"kind"`) — that is a struct flattening one
internally-tagged enum, which schemars handles, and is distinct from flattening
the top-level `Op` enum, which this decision forbids.
