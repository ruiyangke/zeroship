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

## Wire-tag pins (the discriminant strings the JS builder emits)

The `"op"` discriminant for each variant is the camelCased Rust variant name. One
pin is non-obvious and is recorded here so a JS-builder author does not guess:

- **`del()` records `{"op":"delete"}`, NOT `{"op":"del"}`.** The JS DSL exposes
  the delete op-function as `del` (the bare word `delete` is a JS reserved word
  and cannot be a clean named export), but the recorded IR discriminant is the
  full camelCased variant `"delete"` (`Op::Delete` → `rename_all="camelCase"` →
  `"delete"`). The Rust loader, the `op-ir.schema.json` `oneOf` branch
  (`properties.op.const == "delete"`), and the Wave-E exhaustiveness gate all key
  on `"delete"`. A `.ir.json` carrying `{"op":"del"}` is an unknown-variant reject
  at deserialize. (The `Op::Delete` doc-comment in `ir.rs` carries the same pin,
  which flows into the generated schema's branch description.)

## The advisory `checksum` hint field on `MigrationIr`

`MigrationIr` carries an optional `checksum: Option<String>` field (§2.4 point 2):
an ADVISORY integrity hint the builder MAY emit, holding the hex `Checksum::of_ir`
over the hint domain (`ops` + `flags` + `depends_on` + `supersedes` +
`preconditions` — **never** `owner_app`, which is server-stamped and so
unpredictable to the builder). The engine RECOMPUTES and is authoritative; when
present the loader compares its recomputed hint-domain checksum to the hint (a
mismatch is genuine drift). The hint is **EXCLUDED from `Checksum::of_ir`** (just
as `owner_app` is excluded from the hint domain) — folding an artifact's own
checksum into that artifact's checksum would be circular. Because `MigrationIr`
carries `deny_unknown_fields`, the §2.4-permitted hint must be modelled
explicitly or a hint-bearing `.ir.json` would be rejected at deserialize.

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
