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

`IrConstraint` carries its `IrConstraintKind` (itself internally-tagged on
`"kind"`) as a **nested object** (`{"name":…,"kind":{"kind":"fk",…}}`), NOT a
`#[serde(flatten)]` sibling: serde forbids `flatten` together with
`deny_unknown_fields`, and the nested form keeps the strict-unknown-key gate
sound while still emitting a clean, schemars-derivable discriminated union.

## The closed expression AST (`Expr`) — same discipline (§3.3.1)

The transform/predicate positions (`update`/`backfill` `set`, `where`, an
`addCheck` body, a partial-index `where:`) carry the **closed expression AST**
(`crates/zeroship-migrate/src/expr.rs`, `Expr`), internally-tagged via
`#[serde(tag = "node")]` + `rename_all = "camelCase"` (`{"node":"colRef",…}`) —
the identical representation choice as `Op`, for the identical reasons (a stable
discriminant, a schemars-derivable `oneOf`, an unknown node tag rejected at
deserialize). The AST is **constructed in JS and serialized as data, NEVER parsed
from text** — so validation is purely structural (no lexer/parser/fuzzer), and
there is **no raw-SQL escape** anywhere in the IR (property A): no `Op::Raw`, no
`op.raw`/`op.sql`, no SQL-string transform fragment.
