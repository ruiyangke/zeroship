> Archived 2026-05-25: shipped. Live reference: docs/reference/db.md.

# Platform System Fields

**Status**: design proposal.
**Lands after**: P5 (encryption baseline) — AAD upgrades to bind `version` when this lands.
**Affects**: every plugin-db table going forward; existing tables migrated in a one-time pass.
**Inspired by**: Salesforce object metadata (the system-fields-as-naked-names convention), Rails ActiveRecord timestamp magic columns, MongoDB `_id` mandatory PK.

---

## 1. Motivation

Every creator-defined model in plugin-db gets a fixed set of platform-managed fields, mandatory, automatically populated by the runtime. This pattern is industry-standard (Salesforce calls them "system fields"; Rails calls them "magic columns"; MongoDB hardcodes `_id`/`_rev`).

Today plugin-db has exactly one: `id` (typed_id, minted SDK-side). This proposal expands the set to give every table:
- **Audit trail** — who/when for create + update
- **Optimistic concurrency** — `version` counter
- **Soft delete** — `deleted_at` nullable timestamp
- **Cryptographic version binding** — `version` feeds AEAD AAD for the P5+ encryption layer (defence against ciphertext rollback within the same row)

Creators don't opt in. Creators can't opt out. The fields are reserved names; user schemas can't redefine them.

## 2. The field set

```typescript
// Every creator table at runtime ends up with these fields,
// in this declared order, before any user-defined fields:

  id          : t.id(),                      // PK; typed_id; minted SDK-side; immutable
  created_at  : t.timestamp().auto_now(),    // server NOW() at INSERT
  updated_at  : t.timestamp().auto_now_on_update(), // server NOW() at INSERT and every UPDATE
  created_by  : t.actor().nullable(),        // session.actor_id at INSERT (null for system writes)
  updated_by  : t.actor().nullable(),        // session.actor_id at INSERT and UPDATE
  version     : t.integer().default(1),       // 1 at INSERT; bumped by 1 on every UPDATE
  deleted_at  : t.timestamp().nullable(),     // null = live; non-null = soft-deleted at this timestamp
```

Seven fields. Bounded set — this list is unlikely to grow much beyond it.

### 2.1 `id`

Already exists. UUIDv7 + base62 + entity prefix (`usr_…`, `post_…`). Minted via `typed_id::new(prefix)` SDK-side before the row reaches the wire. Immutable post-INSERT.

**This proposal changes nothing about `id`** — it's listed here for completeness so the system-field set is enumerated in one place.

### 2.2 `created_at` / `updated_at`

PostgreSQL: `TIMESTAMPTZ`. SQLite: `TEXT` ISO 8601 (matches the auth/session-minter convention from P3).

Server-side default `NOW()` at INSERT for both. PG uses `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`; SQLite emits an explicit `INSERT INTO ... SET created_at = CURRENT_TIMESTAMP` via the runtime's bind layer (SQLite's `DEFAULT CURRENT_TIMESTAMP` doesn't propagate through the `INSERT … VALUES (...)` form reliably).

`updated_at` is bumped on every UPDATE via a trigger or via the runtime's UPDATE builder appending `updated_at = NOW()` to every SET clause. **Pick the latter** — triggers are PG-specific and the runtime already controls UPDATE SQL emission.

### 2.3 `created_by` / `updated_by`

Actor identifier. Sourced from the current request's `SessionMinter` token (P3): `session.actor_id`. Format: typed_id (`usr_…`, `app_…`, etc.).

Nullable because:
- System-initiated writes (migrations, background jobs, control-plane operations) may have no actor.
- The `auto_*` actor kind covers automation; the actor_id may be `None` for fully anonymous internal writes.

PG: `TEXT`. SQLite: `TEXT`. No FK constraint to a users table — actors can be from many namespaces (`usr_…`, `app_…`, `prj_…`), and we don't want plugin-db's user tables coupled to a platform-wide `actors` table that doesn't exist.

### 2.4 `version`

`INTEGER NOT NULL DEFAULT 1`. Bumped by 1 on every UPDATE (UPDATE SQL builder appends `version = version + 1`). Used for:

- **Optimistic concurrency control**: `db.posts.update({ id: "post_...", version: 5 }, { title: "new" })` — fails with code `version_mismatch` if current `version != 5`. Implementation: `UPDATE ... SET ... WHERE id = $id AND version = $expected` — affected-rows = 0 → typed error.
- **AAD binding for encrypted columns** (P5+): `canonical_aad(collection, column, id_bytes, version_bytes)`. Ciphertext is bound to the row's write generation. An attacker who captures ciphertext at version 5 and tries to replay it after a legitimate update to version 6 gets `encryption_aead_failed`.
- **CDC subscriber idempotency**: subscribers process `(id, version)` tuples; duplicate event delivery is detected via known-`version` checks at the application layer.

### 2.5 `deleted_at`

`TIMESTAMPTZ NULL` (PG) / `TEXT NULL` (SQLite, ISO 8601). Default `NULL`.

**Salesforce uses a boolean `IsDeleted`. This proposal uses a nullable timestamp instead**, for one practical reason: "when was this deleted" is operationally valuable (compliance audits, undo windows, garbage-collection scheduling). A boolean throws that information away.

`db.posts.find({})` implicitly filters `WHERE deleted_at IS NULL`. To include soft-deleted rows: `db.posts.find({}, { include_deleted: true })`. Hard-delete (`db.posts.purge(id)`) is separate from soft-delete (`db.posts.delete(id)` which sets `deleted_at = NOW()`).

## 3. Reserved names

`validate_field_name` (in `crates/plugin-db/src/query.rs`) refuses any creator-defined field whose name is in the system-field list. The error:

```rust
DbError::ValidationFailed {
  code: "reserved_field_name",
  message: format!("Field name '{name}' is reserved for platform system fields"),
  hint: Some("System fields (id, created_at, updated_at, created_by, updated_by, version, deleted_at) are managed by the platform and cannot be overridden.".into()),
}
```

The reserved list is a constant: `const SYSTEM_FIELD_NAMES: &[&str] = &["id", "created_at", "updated_at", "created_by", "updated_by", "version", "deleted_at"];`.

The `__zeroship_*` / `__zs_*` / `_*` prefix reservations (P4) continue to apply for synthetic columns like `_distance`, `_rank`, `_distance_m` — those are *query-result* injections, not stored columns. Two distinct namespaces.

## 4. SDK shape

### 4.1 Creator-facing TypeScript

The creator writes:

```typescript
export default {
  schema: schema((t) => ({
    posts: {
      title: t.string(),
      content: t.string(),
    },
  })),
};
```

The platform installs:

```sql
CREATE TABLE "app_xyz"."posts" (
  id          TEXT PRIMARY KEY,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by  TEXT NULL,
  updated_by  TEXT NULL,
  version     INTEGER NOT NULL DEFAULT 1,
  deleted_at  TIMESTAMPTZ NULL,
  title       TEXT NOT NULL,
  content     TEXT NOT NULL
);
CREATE INDEX "posts__deleted_at_idx" ON "app_xyz"."posts" ("deleted_at"); -- the implicit-filter hot path
CREATE INDEX "posts__updated_at_idx" ON "app_xyz"."posts" ("updated_at"); -- CDC subscriber resume
```

The creator's TypeScript view:

```typescript
const post = await db.posts.insert({ title: "Hi", content: "..." });
// post is typed as:
//   { id: string, created_at: Date, updated_at: Date,
//     created_by: string | null, updated_by: string | null,
//     version: number, deleted_at: Date | null,
//     title: string, content: string }
// — system fields are visible in Row<S> by default.
```

The SDK's `Row<S>` inference (`sdks/db/src/types.ts`) appends the system fields to the row shape automatically. Creators see them; they don't have to declare them.

### 4.2 New SDK methods

```typescript
class Collection<S> {
  // Existing
  insert(value: Insert<S>): Promise<Result<Row<S>>>;
  update(filter: Filter<S>, patch: Update<S>): Promise<Result<Row<S>[]>>;
  delete(filter: Filter<S>): Promise<Result<Row<S>[]>>;  // changes semantics — see below

  // New
  /** Hard-delete: removes the row from storage. For compliance / GDPR-erase use. */
  purge(filter: Filter<S>): Promise<Result<Row<S>[]>>;

  /** Restore soft-deleted: sets deleted_at = null, bumps version + updated_at. */
  restore(filter: Filter<S>): Promise<Result<Row<S>[]>>;

  // Modified
  find(filter?: Filter<S>, opts?: { include_deleted?: boolean }): Promise<Result<Row<S>[]>>;
  // find now auto-filters `deleted_at IS NULL` unless `include_deleted: true`
}
```

**Behaviour change for `delete()`**: today, `delete()` hard-deletes. With system fields, `delete()` becomes a soft-delete (sets `deleted_at = NOW()`). Hard-delete moves under `purge()`. This is a SDK API change that needs a migration note — see §7.

### 4.3 Optimistic concurrency

```typescript
const post = await db.posts.findOne({ id: "post_..." });
// post.version === 5

// Some time passes; maybe someone else updates the row.

// Optimistic update — succeeds only if version still 5:
await db.posts.update(
  { id: "post_...", version: 5 },
  { title: "new title" },
);
// → if current version != 5, throws { code: "version_mismatch", retryable: true }
```

The native dispatch composes: `UPDATE ... SET title = $1, version = version + 1, updated_at = NOW() WHERE id = $id AND version = 5`. Affected-rows check; 0 affected → typed error.

If `version` is omitted from the filter, UPDATE proceeds without the check (last-writer-wins; bumps version anyway).

## 5. Index implications

Three indexes are added implicitly per table:

```sql
CREATE INDEX "<coll>__deleted_at_idx"   ON "<app>"."<coll>" ("deleted_at");  -- find() hot path
CREATE INDEX "<coll>__updated_at_idx"   ON "<app>"."<coll>" ("updated_at");  -- CDC sync
CREATE INDEX "<coll>__created_by_idx"   ON "<app>"."<coll>" ("created_by");  -- "my items" queries
```

These are NOT user-configurable. They're not part of the creator's index spec; they're tied to the system fields' presence.

`PostgresBackend`'s `IndexBuilder` emits them via `CREATE INDEX CONCURRENTLY IF NOT EXISTS`. SQLite emits plain `CREATE INDEX IF NOT EXISTS`. Both idempotent.

## 6. CDC interaction

`ChangeEvent`s (from P2) now naturally carry the system field set. Subscribers see:

```typescript
event = {
  collection: "posts",
  op: "update",
  pk: "post_...",
  new_tuple: { id, created_at, updated_at, created_by, updated_by, version, deleted_at, title, content },
  old_tuple: { ...same shape with pre-update values... }
}
```

Subscriber benefits:
- **Idempotency**: `(pk, version)` is the natural dedup key.
- **Filter on logical state**: subscribers can ignore events where `new_tuple.deleted_at != null && old_tuple.deleted_at == null` ("soft-delete event") if they only care about active rows.
- **Resume from a point in time**: subscribers store `last_seen_updated_at` and request `WHERE updated_at > last_seen` after reconnect.

Soft-delete IS still a CDC event — subscribers see `op: "update"` with `new_tuple.deleted_at` flipping from null to a timestamp. Hard-delete (purge) is a CDC event with `op: "delete"`.

## 7. Migration story

### 7.1 Existing tables (the breaking change)

Tables created before this lands don't have the system fields. The migration needs to:

1. **Detect existing tables** by walking `__zeroship_migrations` (or the per-backend introspection).
2. **For each table**: `ALTER TABLE ... ADD COLUMN created_at TIMESTAMPTZ DEFAULT NOW()` etc. The DEFAULT NOW() backfills existing rows with the time of the migration — best-available approximation; not historically accurate.
3. **Create the implicit indexes** via `CREATE INDEX CONCURRENTLY IF NOT EXISTS`.
4. **Mark the table as migrated** via an audit row in `__zeroship_migrations`.

For `version`, existing rows get `1`. For `created_by`/`updated_by`, existing rows get `NULL` (unknown actor). For `deleted_at`, existing rows get `NULL` (alive).

The migration is run by `register_model` Pass 2 on first deploy after the platform upgrade. It's append-only DDL (additive — strict mode allows it; per the diff classifier this is `Additive` change).

### 7.2 `delete()` semantics change

`db.posts.delete(...)` semantics shift from hard-delete to soft-delete in this release. The migration release note **must** call this out — apps relying on `delete()` for compliance / hard-erase need to migrate to `purge()`.

The native runtime can detect calls to `delete()` on a table without `deleted_at` (i.e., pre-migration table) and fall back to hard-delete with a `tracing::warn!` so older apps don't break instantly. The warn becomes an error in the next major version once the platform-wide migration completes.

## 8. Encryption / AAD interaction

When P5 (encrypted columns) lands, the AAD construction for randomised mode upgrades:

```rust
// P5 baseline (Camp A — pre-system-fields):
let aad = canonical_aad(collection, column, Some(row_pk_bytes));

// Post-system-fields:
let aad = canonical_aad(collection, column, Some(row_pk_bytes), Some(version_bytes));
```

`canonical_aad` gets a fourth length-prefixed block for `version`. **Ciphertext format gains a version flag** (one byte in the header) to allow co-existence of pre-system-fields and post-system-fields ciphertexts during the migration window. The decryption path inspects the flag and reconstructs AAD accordingly:

```rust
match flag {
    0x01 => canonical_aad(coll, col, Some(row_pk), None),                  // P5 baseline
    0x02 => canonical_aad(coll, col, Some(row_pk), Some(version_bytes)),    // post-system-fields
    _    => Err(unknown_ciphertext_version),
}
```

This enables a **rolling re-encryption** path:

1. New writes use flag `0x02` (version-bound AAD).
2. Old reads accept flag `0x01` (backward-compat).
3. A background re-encryption job walks every encrypted column, decrypts under `0x01`, re-encrypts under `0x02`, advances the row's version.
4. After all rows are migrated, the system can refuse `0x01` ciphertext (operator decision, post-migration).

This costs no extra storage (one byte in the ciphertext header, fixed). It costs one explicit migration phase. **P5's design accommodates this** — the wire format already specifies a "no sentinel prefix" choice; this proposal revisits that for the version flag specifically.

The defence-in-depth benefit: an attacker who captures ciphertext at `version=5` and replays it after a legitimate `version=6` update gets `encryption_aead_failed` — the "ciphertext rollback within the same row" attack is now blocked.

## 9. Commit sequence

This is a platform-wide feature; sequenced as its own phase (call it **P7** post-P5/P6, or run in parallel with P6 if scheduling allows).

### PR 1 — Schema DSL + reserved-name validator + Cargo

- `sdks/db/src/types.ts`: add `t.id()`, `t.timestamp()`, `t.actor()`, the `auto_now()` / `auto_now_on_update()` modifiers. Refuse field names colliding with `SYSTEM_FIELD_NAMES`.
- `crates/plugin-db/src/query.rs`: add `SYSTEM_FIELD_NAMES` constant; `validate_field_name` rejects them.
- `Row<S>` type inference includes system fields automatically.
- Gate: schema-validation tests refuse `t.string()` named `id`, `created_at`, etc.

### PR 2 — CREATE TABLE includes system fields + indexes

- `query.rs::build_create_table_with_fks`: prepends the 7 system-field columns to every CREATE TABLE.
- Auto-emits the 3 implicit indexes (`deleted_at`, `updated_at`, `created_by`).
- Both backends (PG dialect, SQLite dialect) emit the equivalent DDL.
- Gate: integration test asserts a freshly-registered model has the system fields + indexes.

### PR 3 — INSERT auto-populates system fields

- `crud::dispatch_insert`: before validation, populate `id` (if missing) via `typed_id::new(prefix)`; populate `created_by` / `updated_by` from `SessionMinter` context; let DB DEFAULT NOW() handle `created_at` / `updated_at`; `version = 1`; `deleted_at = NULL`.
- Existing tests should pass (system fields populate automatically without test changes).
- Gate: insert returns a row with all 7 system fields populated.

### PR 4 — UPDATE auto-bumps version + updated_at + optimistic concurrency

- `crud::dispatch_update`: append `version = version + 1, updated_at = NOW(), updated_by = $session_actor` to every UPDATE SET clause.
- If `filter` contains `version: N`, the WHERE clause includes `AND version = N`; check affected-rows; 0 → `version_mismatch`.
- Gate: `optimistic_concurrency_blocks_stale_update`, `update_bumps_version_and_updated_at`.

### PR 5 — delete()/purge()/restore() + find() auto-filter

- `crud::dispatch_delete`: now soft-deletes (`UPDATE ... SET deleted_at = NOW(), version = version + 1`).
- New `crud::dispatch_purge`: hard DELETE.
- New `crud::dispatch_restore`: clears `deleted_at`, bumps version.
- `crud::dispatch_find`: appends `AND deleted_at IS NULL` unless `include_deleted: true`.
- SDK exposes `purge()`, `restore()`, `find(filter, { include_deleted })`.
- Gate: `soft_delete_hides_from_find`, `include_deleted_returns_soft_deleted`, `purge_removes_row_permanently`.

### PR 6 — Existing-table migration (one-time pass)

- `register_model::apply` Pass 2 detects tables without system fields (introspect the live schema, look for absence of `version` column) and ALTERs them in.
- Backfills `version = 1`, `deleted_at = NULL`, `created_at = NOW()` (time-of-migration), etc.
- Gate: `existing_table_migrates_to_system_fields_idempotently`.

### PR 7 — Docs + design amendment

- `docs/reference/db.md`: System Fields section.
- `docs/proposals/db-system-design.md`: amendment block (top of doc) + §6 + §15 cross-references to this proposal.
- `docs/proposals/p5-encryption-backup-implementation-plan.md`: §8 AAD upgrade note — `version` now bound when system fields are available.

## 10. Open questions

| # | Question | Default |
|---|---|---|
| Q-SF-A | `created_by` actor format — typed_id only, or allow arbitrary string? | Typed_id only. Reject non-typed-id values at INSERT. |
| Q-SF-B | Should `id` accept user-supplied values, or always auto-mint? | Allow user-supplied IF the value passes typed_id format validation. Common pattern for inserts where the caller knows the ID (e.g., idempotency keys). |
| Q-SF-C | Should `find()` join the soft-delete filter into the index, or rely on a partial index `WHERE deleted_at IS NULL`? | Plain B-tree on `deleted_at`. Partial indexes are PG-specific (SQLite has them too but with caveats). |
| Q-SF-D | `restore()` after the soft-delete TTL expired — allowed? | Yes, no TTL enforced at this layer. TTL-based hard-cleanup is a separate background job (P8+?). |
| Q-SF-E | Optimistic-concurrency error: retryable or not? | `retryable: true` — application can re-read and retry with the new version. |
| Q-SF-F | What if a table has both an encrypted column AND a unique constraint? Encrypted columns + unique already lands in P5. Does the system-field migration break it? | The migration adds columns; unique constraints unaffected. |
| Q-SF-G | Schema migration: backfill `created_at` from existing rows' inferred age (if any)? | No — too lossy. Just stamp the migration time. Document the limitation. |
| Q-SF-H | Should `purge()` also remove the row from CDC stream history? | No — CDC is an audit signal; we want the purge event to fan out so subscribers can clean up their caches. The deleted event carries the final state. |
| Q-SF-I | `t.actor().nullable()` vs `t.actor()` with implicit nullable? | Explicit `nullable()` — matches the SDK convention. |
| Q-SF-J | What about `db.foo.count()`, `aggregate()`, etc. — do those auto-filter `deleted_at`? | Yes — every read-side method auto-filters. Consistency. |

## 11. Riskiest decision

**Q-SF-Default: `delete()` semantics shift from hard to soft.**

Today, `db.posts.delete({id: x})` removes the row from storage. After this proposal lands, the same call sets `deleted_at = NOW()` and the row stays in storage (just hidden from `find()`).

This is a behaviour change for existing creators. Three options:

**(A) Just change it, document loudly.** Most existing apps probably don't care — they're not relying on hard-delete semantics. Migration note in the release. SDK version bump to communicate intent.

**(B) Add a third method, leave `delete()` alone.** `delete()` stays hard-delete; soft-delete is opt-in via `soft_delete()` and `purge()` is unchanged. But then soft-delete isn't the default behaviour, defeating the value proposition.

**(C) Detect-and-warn**: `delete()` does soft-delete on tables that have `deleted_at` (post-migration); does hard-delete on tables that don't (pre-migration). Eventually deprecate hard-delete fallback. Bridges the migration.

**Recommendation: (C)**. Smooth migration path, no breaking change for existing apps until the platform-wide migration completes. The `tracing::warn!` for legacy hard-delete-via-`delete()` becomes a soft-fail in the deprecation window, then a hard error in a future major version.

The risk if reviewers reject (C): we either ship a breaking change (A) or split the API surface (B). (C) is the engineering-clean path.

---

## Status: SHIPPED (partial) — 2026-05-24

| PR | Commit     | Status                              | Scope                                                                  |
|----|------------|-------------------------------------|------------------------------------------------------------------------|
| 1  | `8ec76868` | Landed                              | Schema DSL + reserved-name validator for the 7 platform system fields. |
| 2  | `8f9f1e6e` | Landed                              | CREATE TABLE prepends 7 system fields + 3 auto-indexes (PG + SQLite).  |
| 3  | `bf1cce58` | Landed                              | INSERT auto-populates system fields + `id:string` cascade + FK type fix. |
| 4  | `8a296728` | Landed                              | UPDATE auto-bumps `version` + `updated_at` + optimistic concurrency.   |
| 5  | `c38ff4de` | Landed (Path C detect-and-warn)     | `delete()` becomes soft-delete; add `purge()` + `restore()`; `find()` auto-filters `deleted_at`. |
| 6  | —          | **Deferred to post-launch**         | Existing-table migration. See below.                                   |
| 7  | this PR    | Landed                              | Docs (`db.md` system-fields section) + design-doc amendment.           |

### PR 6 — deferred to post-launch

The platform is **pre-launch as of 2026-05-24**: never published, no
production users, no production tables. PR 6 was the one-time
`ALTER TABLE … ADD COLUMN` migration for tables created before PR 2
landed — but there are no such tables in production to migrate. The
PR is therefore deferred indefinitely.

Path C (detect-and-warn for tables without `deleted_at`) shipped with
PR 5 and remains the safety net for any pre-PR-2 tables that
materialise in dev/test environments. A real PR 6 — designed against
real schema-evolution data — lands when there are production schemas
to evolve against.

### P7.5 — AAD upgrade (pending)

The AAD-with-`version` upgrade described in §8 is **unblocked but
not yet shipped**. PR 4 put `version` on every row that exists (every
table created after PR 2 carries it; PR 6's existing-table backfill is
moot per the pre-launch deferral above). The remaining work is:

1. Extend `canonical_aad(collection, column, row_pk_bytes,
   version_bytes)` — the signature is already documented in §8.
2. Bump the ciphertext wire-format flag from `0x01` → `0x02`.
3. Land rolling re-encrypt-on-write: new writes use `0x02`; reads
   accept both `0x01` and `0x02` during the cutover window.

Pre-launch posture means no `0x01` production ciphertext exists, so
step 3's backward-compat read path can ship as a same-PR cutover (no
extended deprecation window required). This PR is tracked in the queue
as **P7.5** and is the highest-leverage encryption hardening remaining
before P6a / P6b.

### Pre-launch simplification (open question, deferred)

PR 5's Path C legacy-warn arm and P5.5 PR 8's `scan-mask-usage` CLI
are **dead-code-in-practice** given the pre-launch posture: no creator
code exists to scan, no pre-system-fields tables exist to warn about.
A future "pre-launch simplification" PR can rip them out, simplifying
the dispatch path. **Not in scope for this PR** — user did not request
it in the current cycle.
