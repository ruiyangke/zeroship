# Case-insensitive text — design (DSL v2, all vendors)

**Status:** approved for build 2026-07-03. Portable intent node — PG/SQLite/MySQL.
**Goal:** author case-insensitive text columns structurally (`t.text({ caseSensitive: false })`), clearing the
7 platform `public.citext` raws (raw 12→5), and giving creators a portable case-insensitive-text primitive.

## Surface — a facet on text, NOT a `t.citext()` type

`t.text({ caseSensitive: false })`. Default `caseSensitive: true` (normal text). "Case-insensitive" is a
*property* of a text column, and the underlying mechanism differs per engine (PG uses a distinct TYPE, SQLite
/ MySQL use a COLLATION), so the facet lets the engine own that mapping — a portable intent node.

## Per-vendor lowering

| dialect | mechanism | `caseSensitive: false` renders | note |
|---|---|---|---|
| Postgres | distinct type | `public.citext` | requires `CREATE EXTENSION citext` (platform declares it, public schema) |
| SQLite | column collation | `text COLLATE NOCASE` | ASCII-only folding (documented divergence, same class as citext under C locale) |
| MySQL | column collation | `text` (default `_ci`) | INVERTED default — MySQL text is already case-insensitive; no explicit COLLATE needed |

The MySQL inversion: PG/SQLite default case-SENSITIVE, MySQL defaults case-INsensitive (`_ci` collations). So
`caseSensitive:false` ADDS on PG/SQLite but is the DEFAULT (no-op) on MySQL — exactly why an intent node is
right (creator says it once, engine does the per-dialect-correct thing).

## Engine surface

1. `IrColumn.case_sensitive: Option<bool>` facet (serde rename `caseSensitive`, `skip_serializing_if` None).
   Only `Some(false)` is meaningful/emitted; absent/`true` ⇒ BYTE-IDENTICAL to today (checksum-stable).
2. Render (declarative.rs, column-TYPE emission): a `ColType::Text` column with `caseSensitive:Some(false)` →
   - PG: `public.citext`  (schema-qualified to where the extension lives; matches pg_dump/baseline)
   - SQLite: `text COLLATE NOCASE`
   - MySQL: `text` (rely on default `_ci`; no explicit COLLATE)
   The facet only modifies the TYPE token; everything else (nullable/default/unique) unchanged.
3. Validate: `caseSensitive:Some(false)` valid ONLY on `ColType::Text` (a text column). Reject fail-closed on
   any other ColType (int/json/uuid/etc.) with a clear error.
4. Drift (drift.rs `canonical_extension_type`): a live PG column whose `USER-DEFINED` type is `citext` /
   `public.citext` recovers back to `ColType::Text` + `caseSensitive:false`, so a citext column round-trips with
   ZERO spurious drift. (The facet is a REAL, catalog-recoverable attribute — like the partition facets.)
5. Surface: `t.text({ caseSensitive: false })` in BOTH `sdks/migrate/src/ops.ts` and the lock-step twin
   `crates/zeroship-migrate/src/frontend/migrate_ops.js`. `t.text()` with no arg or `{caseSensitive:true}` emits
   no facet (byte-identical). Update generated types.

## Extension dependency (decision)

REQUIRE the `citext` extension to be declared in the migration (the platform does, public schema). The render
emits `public.citext`; if the extension is absent, apply fails — the creator's responsibility, matching the
platform. Auto-emitting `CREATE EXTENSION` per column is out of scope (a possible future ergonomic).

## Platform re-author (slice B)

The 7 `ALTER COLUMN <col> TYPE public.citext` raws → author the column as `t.text({ caseSensitive: false })`
in its create() block, delete the raw. The columns (all email): email_suppressions.email,
email_verifications.email, federated_identities.email_at_link, gateway_sessions.email, magic_completions.email,
magic_links.email, users.email. Faithfulness: `public.citext` renders identically → pg_dump differential 0.

## Verify

- Slice A: `nix develop -c cargo test -p zeroship-migrate --tests --no-fail-fast -- --test-threads=1`
  (--no-fail-fast MANDATORY) + `pnpm --filter @zeroship/migrate build && test`.
- Slice B: pg_dump DIFFERENTIAL (baseline raw-12 vs new raw-5 → fresh DBs via `migrate --profile platform` →
  `pg_dump --schema-only -n zeroship` → diff ignoring `\restrict`) = semantic 0. Live PG :5440; NEVER concurrent.
- Regression tests: render (PG citext / SQLite NOCASE), validate (reject on int/json), serde-omission, recorder
  (`t.text({caseSensitive:false})` emits facet; `t.text()` omits), drift round-trip (citext col → 0 drift).

## Slices
- **A — engine + surface** (facet + render + validate + drift-recovery + surface twin + tests). Verify nix + pnpm.
- **B — platform re-author** (7 email columns) + differential pg_dump 0-diff. raw 12→5.
