# Full PG-vendor sequence support — design (DSL v2)

**Status:** approved for build 2026-07-04. PG-vendor, fail-closed on SQLite/MySQL.
**Goal:** close the last-mile sequence-attachment gaps so the platform's 2 sequence raws go structural
(raw 5→3), and PG sequence support is complete.

## What already exists (no work)

- Sequence objects: `Op::CreateSequence` / `AlterSequence` / `DropSequence` + `sequence(name).create/alter/drop`
  surface, incl. `alter({ ownedBy: { table, column } })`.
- Identity columns (create-time): `IdentityCol { always }` + `t.*.identity({ always })` → renders
  `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` (`pg_identity_clause`).

## The gap — one engine addition

Only the **`nextval(sequence)` column default** is missing. The platform's `audit_events.id` is SERIAL-style:
an explicit named sequence (`audit_events_id_seq`, created + OWNED BY already structural) plus a column
`DEFAULT nextval('zeroship.audit_events_id_seq'::regclass)` — that default is the raw.

### `IrDefault::Nextval`

```rust
IrDefault::Nextval { sequence: SequenceRef }        // SequenceRef { name: String, schema: Option<String> }
```
A CLOSED sequence reference (a name + optional schema), never raw SQL — fits the frozen-IR contract.

- **Render (PG):** `nextval('<schema>.<name>'::regclass)` (schema-qualified to match pg_dump/baseline;
  `regclass` cast as pg_dump emits).
- **Validate:** valid only on an integer column (`Int` / `BigInt` / `SmallInt`) AND PG dialect — a sequence
  default is PG-vendor; SQLite/MySQL have no standalone sequences → reject fail-closed.
- **Drift:** a live column whose default introspects as `nextval('…'::regclass)` recovers to
  `IrDefault::Nextval { sequence }` (parse the sequence name out of the `nextval(...)` default text), so an
  audit_events-style column drifts zero.
- **Surface:** a `nextval(name, { schema? })` builder passed to `.default()`:
  `id: t.bigInt().notNull().default(nextval("audit_events_id_seq", { schema: "zeroship" }))`.
  The recorder's `toIrDefault` detects the nextval marker → the variant. Lock-step `ops.ts` ↔ `migrate_ops.js`.

## Platform re-author (slice B)

Both in `db/migrations-ts/20260702000300_auth_oauth_tables.ts`:
1. `audit_events.id`: `t.bigInt().notNull()` → `t.bigInt().notNull().default(nextval("audit_events_id_seq", { schema: "zeroship" }))`; delete the `SET DEFAULT nextval` raw. (Sequence create/ownedBy already structural.)
2. `totp_backup_codes.id`: `t.bigInt().notNull()` → `t.bigInt().notNull().identity({ always: true })`; delete the
   `ADD GENERATED ALWAYS AS IDENTITY` raw. (No engine change — create-time identity reproduces the catalog: PG
   auto-names `totp_backup_codes_id_seq`, START 1 / INCREMENT 1 / CACHE 1 defaults = the baseline's explicit values.)

## Cross-vendor note (why PG-vendor)

Standalone named sequences + `nextval` defaults are genuinely PG-first: SQLite has no sequence object
(auto-increment is `INTEGER PRIMARY KEY AUTOINCREMENT`), MySQL 8 has no `SEQUENCE` (only `AUTO_INCREMENT`). A
*portable* auto-increment would be a separate intent (`t.serial()` → IDENTITY/AUTOINCREMENT/AUTO_INCREMENT); it
would NOT reproduce the platform's explicit *named* sequence, so it can't clear `audit_events` faithfully.
Hence: PG-vendor `nextval`, fail-closed elsewhere.

## Verify
- Slice A: `nix develop -c cargo test -p zeroship-migrate --tests --no-fail-fast -- --test-threads=1`
  (--no-fail-fast) + `pnpm --filter @zeroship/migrate build && test`. If ColumnSnapshot Debug format changes,
  regen the refactor_safety goldens (`UPDATE_SNAPSHOT_GOLDENS=1`). NO CURRENT_IR_VERSION bump.
- Slice B: pg_dump differential (baseline raw-5 vs new raw-3 → fresh DBs, `pg_dump -n zeroship`, diff ignoring
  `\restrict`) = semantic 0. Live PG :5440; NEVER concurrent.
- Regression: render (nextval PG), validate (reject on non-int / non-PG), serde-omission, drift round-trip
  (nextval default → 0 drift), recorder (`.default(nextval(...))` emits the variant).

## Slices
- **A — engine + surface** (IrDefault::Nextval + render + validate + drift recovery + `nextval()` surface twin +
  tests). Verify nix + pnpm.
- **B — platform re-author** (audit_events nextval default + totp identity) + differential. raw 5→3.
