# jsonb default VALUE — design (DSL v2)

**Status:** approved for build 2026-07-04 ("build very carefully"). Clears the platform's 1 non-empty jsonb
default raw (`net_policy_limits_json`), raw 3→2, and gives creators arbitrary jsonb column defaults.

## The one hard problem: checksum stability

The IR is frozen + checksummed, and the checksum must MATCH across the Rust engine and the JS recorder (two
independent serializers). So the same logical value must produce ONE canonical form regardless of authored key
order / whitespace / number spelling. The IR already solved this for scalars (`IrScalar`: `Int` with `|v|<2^53`,
`Decimal` as a canonical string) — we reuse that discipline; we do NOT invent a JCS canonicalizer.

## Design — a canonical JSON value, added ALONGSIDE the empty-container variant

Keep `IrDefault::Container` (empty `{}`/`[]`) untouched — do NOT churn the shipped/proven container-default work.
Add a sibling variant for NON-empty values:

```rust
// A canonical JSON value. Objects use BTreeMap => deterministic key order.
enum IrJsonValue {
    Null,
    Bool(bool),
    Int(i64),                              // |v| < 2^53, same bound as IrScalar::Int
    Str(String),
    Array(Vec<IrJsonValue>),               // order significant (preserved)
    Object(BTreeMap<String, IrJsonValue>), // keys SORTED => checksum-stable
}

IrDefault::Json { value: IrJsonValue }     // wire: {"json": <value, object keys sorted>}
```

### v1 scope (careful — covers the platform, defers the risky tail)

- **Numbers: integers ONLY** (`Int`, `|v| < 2^53`). A non-integer / float value is REJECTED at record time with a
  clear error. Rationale: float canonicalization is the ONE place a subtle JS↔Rust checksum divergence could hide,
  and the platform's `net_policy_limits_json` is all integers. Float support is a deliberate follow-up (would carry
  `Decimal(canonical string)` like `IrScalar`, once the cross-impl number-format agreement is nailed + tested).
- **Column type: `ColType::Json` ONLY** in v1 (a jsonb literal). A NON-empty value on a `text[]` column (a SQL
  array literal with element escaping) is REJECTED — documented limit; no platform text[]-value default exists.
  (Empty `[]` on text[] still works via the unchanged `Container` path.)
- Reject bytes and any function/synth value nested at any depth (the recorder already scans for nested functions).

### Serialization (the checksum-critical contract)

- `IrJsonValue::Object` → a JSON object with keys in `BTreeMap` (lexicographic, UTF-8 byte) order.
- `IrJsonValue::Int` → a JSON integer.
- The JS recorder MUST build the value with the SAME rules: sort object keys, coerce a JS number that is
  `Number.isInteger(v) && Math.abs(v) < 2**53` to an integer, else THROW (v1 float rejection). Recurse into
  arrays/objects; reject nested functions.
- Round-trip: deserialize→reserialize is byte-identical; a column WITHOUT a Json default is byte-identical to today
  (skip-when-absent). NO `CURRENT_IR_VERSION` bump.

## Surface

`.default({ max_sockets: 4, egress_ceiling_bytes: 10485760 })` — a real JS object/array. The recorder
(`toIrDefault` in `ops.ts` + lock-step `migrate_ops.js`) replaces the current `NON_EMPTY_CONTAINER_DEFAULT_ERROR`
throw with building `IrJsonValue`. PRESERVE the existing branches: empty `{}`/`[]` → `Container`; the explicit
scalar carriers (`{decimal:…}` / `{bytes:…}`) → `Literal`; nested-function rejection stays.

## Render — ColType-dispatched (faithfulness is trivial; PG normalizes jsonb)

- Postgres: `'<compact-canonical-json>'::jsonb` (keys in the same sorted order; PG re-normalizes on storage, so the
  pg_dump differential passes regardless of exact spacing).
- MySQL: `DEFAULT (CAST('<json>' AS JSON))` (MySQL requires the parenthesized-expression form for a JSON default —
  same rule the empty-container transform already encodes as `JSON_OBJECT()`/`JSON_ARRAY()`).
- SQLite: `DEFAULT '<json>'` (json stored as text).

## Validate

`IrDefault::Json` valid only on `ColType::Json`; a non-integer number is rejected; a Json value on any non-json
ColType is rejected. Mutually exclusive with the other default variants (a column has one default).

## Drift — NONE needed

`ColumnSnapshot::PartialEq` deliberately EXCLUDES `default` (defaults are DDL-emission metadata, not drift-compared;
PG normalizes them). So a jsonb-value default cannot phantom-drift and needs no introspection recovery. (Verified:
snapshot.rs PartialEq compares name/data_type/nullable/case_sensitive/comment only.)

## Tests (TDD)

1. **Checksum stability (the guard):** authoring `{a:1, b:2}` and `{b:2, a:1}` produce the IDENTICAL IR + IDENTICAL
   `Checksum::of_ir`. Nested: `{outer:{z:1, a:2}}` canonicalizes inner keys too.
2. Render: a json column with `Json{Object{...}}` renders `'{"egress_ceiling_bytes": 10485760, "max_sockets": 4}'::jsonb`
   (PG); MySQL CAST form; SQLite text form.
3. Validate: a Json value on an int column rejected; on a text[] column rejected (v1); a FLOAT value rejected at record.
4. Serde: `Json` round-trips; absent = byte-identical.
5. Recorder (`ops.test.ts`): `.default({a:1})` emits `{json:{a:1}}` with sorted keys; `.default({})` still emits
   `{container:"object"}`; `.default({b:2,a:1})` == `.default({a:1,b:2})`; a float default throws; public-vs-engine parity.

## Verify (extra rigor for a checksum-sensitive change)

- Full `nix develop -c cargo test -p zeroship-migrate --tests --no-fail-fast` + `pnpm --filter @zeroship/migrate
  build && test`. Regen op-ir.schema.json (UPDATE_SCHEMA=1) / snapshot goldens (UPDATE_SNAPSHOT_GOLDENS=1) / corpus
  (UPDATE_CORPUS=1) as the new variant requires.
- **code-critic review** of the canonicalization + checksum diff before committing (dual-review; the risky part).
- Re-author slice: `net_policy_limits_json` → `.default({ max_sockets: 4, egress_ceiling_bytes: 10485760 })` +
  delete the raw → pg_dump differential = 0 → raw 3→2.

## Slices
- **A — engine + surface + tests** (IrJsonValue + IrDefault::Json + render + validate + recorder twin). Verify + critic.
- **B — re-author `net_policy_limits_json`** + pg_dump differential. raw 3→2.
