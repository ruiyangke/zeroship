# Empty-container column default — design (DSL v2, platform migration)

**Status:** approved for build 2026-07-03. Continues the platform raw-marker reduction.
**Goal:** author the platform's empty-container column defaults structurally, clearing 11 raw markers
(`SET DEFAULT '{}'::jsonb` / `'[]'::jsonb` / `'{}'::text[]`), platform pg_dump semantic diff = 0.

## Scope

Target = the **11 EMPTY-container** `SET DEFAULT` raws:
- `'{}'::jsonb` ×5 (json columns: sandbox_events.data, + detail, metadata ×2, usage_at_change)
- `'[]'::jsonb` ×2 (json columns: gated_versions, migration_versions)
- `'{}'::text[]` ×4 (text[] columns: amr, granted_scopes ×2, matched_policies)

OUT of scope (stay raw): `net_policy_limits_json` (a NON-empty `'{...}'::jsonb` — arbitrary JSON default value,
the systemic gap), and `id SET DEFAULT nextval(...)` (sequence/serial — a different category).

## The limit

`IrDefault` = `Literal { value: IrScalar }` | `Fn { fn: SynthDefaultFn }`. `IrScalar` is Null/Bool/Int/
Decimal/Str/Bytes — deliberately container-free (a CHECK/scalar literal must not be an object/array). So
`.default({})` / `.default([])` have no representation and are rejected. The confined "default defaults"
synth (`declarative.rs:1337`, `synth_json_defaults`) already emits `'{}'::jsonb`/`'[]'::jsonb`, but it is an
AUTO path gated OFF for the platform profile — platform columns need an EXPLICIT default, hence the raws.

## Design — a new IrDefault variant (empty-only, closed)

`IrDefault::Container { kind: EmptyContainerKind }`, `enum EmptyContainerKind { Object, Array }`
(camelCase wire, tagged single-key object like `Fn` — e.g. `{ "container": "object" }`). Empty-only; a
non-empty object/array stays unrepresentable (the systemic-value gap, fail-closed with a clear error).

**Render** (column-type-aware — the IrColumn carries `ty: ColType`):
- `Container{Object}` → `'{}'::jsonb`  (valid only on `ColType::Json`)
- `Container{Array}`  → `'[]'::jsonb`  on `ColType::Json`;  `'{}'::text[]` on `ColType::TextArray`
- any other (ColType, kind) → **reject at validate**

These spellings are byte-identical to the baseline and to what `zeroship-schema` / `installSchema` emit, so
faithfulness holds by construction (proven by the differential).

**Validate:** `Object` requires `Json`; `Array` requires `Json` or `TextArray`. Fail-closed otherwise.

## JS surface

`.default({})` → `Container{Object}`; `.default([])` → `Container{Array}`. The recorder's `toIrDefault`
(in BOTH `sdks/migrate/src/ops.ts` and the lock-step twin `crates/zeroship-migrate/src/frontend/
migrate_ops.js`) detects an EMPTY plain object / EMPTY array and emits the variant; a NON-empty container
throws `"non-empty container defaults are not supported yet; only {} and [] are"`. Update
`sdks/migrate/src/generated/{enums,ir}.ts` + types to match.

## Platform re-author (slice 2)

Move each of the 11 raw `ALTER COLUMN x SET DEFAULT ...` into an inline `.default({})` / `.default([])` on
the column in its `create()` block (json → `{}` / `[]`; text[] → `[]`). The catalog is identical (the column
gets the same default) whether authored inline or via ALTER, so pg_dump is unchanged. sandbox_events.data's
default becomes inline on the partitioned parent (inherited by children — removes the last sandbox_events raw).

## Verify

- Engine slice: `nix develop -c cargo test -p zeroship-migrate --tests --no-fail-fast -- --test-threads=1`
  (--no-fail-fast MANDATORY) + `pnpm --filter @zeroship/migrate build && test`.
- Re-author slice: the pg_dump DIFFERENTIAL proof (build target/debug/zeroship-migrate --features
  standalone-cli; apply OLD `git show <hash>:db/migrations-ts/<f>` vs NEW to two fresh DBs via
  `migrate --dir --profile platform`; `pg_dump --schema-only -n zeroship`; diff ignoring the `\restrict`
  token) → semantic diff 0. Live PG = docker appbase-migrate-postgres-1 :5440; NEVER concurrent live-PG.
- Regression tests: render (object→'{}'::jsonb / array→'[]'::jsonb / text[]-array→'{}'::text[]), validate
  (Object-on-text[] rejected), recorder (.default({})/.default([]) emit Container; non-empty throws), serde
  round-trip (absent = byte-identical).

## Slices
- **A — engine + surface** (IrDefault::Container + render + validate + JS .default({})/.default([]) + twin +
  generated + tests). Land together (recorder + engine coupled by contract tests). Verify nix + pnpm.
- **B — platform re-author** (11 raws → inline defaults) + differential pg_dump 0-diff proof.
