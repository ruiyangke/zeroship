# P5 — Encryption + Backup/Restore — Implementation Plan

**Status**: design (planning only — no Rust written).
**Scope**: §19 P5 of `docs/proposals/db-system-design.md`. Two capability traits (`EncryptedColumn` + `Backup`), two backends each, SDK extension, six test gates.
**Prereqs landed**: P0 (capability split), P1 (SQLite core), P2 (CDC), P3 (auth), P4 (search) — 13 capability traits at HEAD `62bfc295`.

---

## 0. Source-code inventory

| Surface | At HEAD | What P5 changes |
| --- | --- | --- |
| `backend/mod.rs` | 13 capability traits; `BackendHandle = Postgres \| Sqlite` enum | **+2 traits**: `EncryptedColumn`, `Backup`. NOT on `Backend` super-trait. NOT on `RegisterBackend`. 2 new `BackendHandle::as_encrypted_column_pg/_sqlite` + `as_backup_pg/_sqlite` accessors (mirror `as_change_stream_*` pattern). |
| `backend/postgres.rs` | All P0–P4 impls | Append `impl EncryptedColumn for PostgresBackend` + `impl Backup for PostgresBackend`. PG impl reads/writes `BYTEA`; `Backup` shells out to `pg_basebackup`/`pg_dump`. |
| `backend/sqlite/*.rs` | 9 sub-modules | NEW `backend/sqlite/encryption.rs` (thin glue → `crate::encryption`); NEW `backend/sqlite/backup.rs` (`VACUUM INTO`). `dialect.rs` gains `encrypted` arm. |
| `crates/plugin-db/src/encryption/` | does NOT exist | NEW module: `mod.rs`, `aead.rs` (AES-256-GCM + deterministic-IV-via-HMAC), `keys.rs` (HKDF + KeyStore), `aad.rs`, `wire.rs`. |
| `diff.rs::ColumnInfo` | 7 fields after P4 | + `encryption: Option<EncryptionMeta>`. |
| `query.rs` | `IndexKind` enum, `build_create_indexes`, column-DDL emitter | Encrypted column DDL → `BYTEA`/`BLOB`. Deterministic mode adds `IndexKind::BTree` over ciphertext bytes. Filter lowering: belt-and-braces fence for randomised-mode (SDK should reject first). |
| `v8_classes/collection.rs` | 16 `#[v8_method]`s | No new methods. Encryption is transparent. |
| `crud.rs` | `dispatch_insert/update/find` | Add per-row encrypt-on-write / decrypt-on-read pass via new `crud/encryption_pass.rs`. |
| `sdks/db/src/types.ts` | TypeBuilder, FieldDef | + `t.encrypted(opts?)` builder + `FieldDef.encrypted`. |
| `sdks/db/src/collection.ts` | filter methods | + `validateEncryptedFieldsInFilter` pre-flight (IMPORTANT #1 fence). |
| `crates/plugin-db/Cargo.toml` | `sha2`, `hmac` (sqlite-optional), `bytemuck` (sqlite-optional) | + `aes-gcm = "0.10"` (workspace; PG and SQLite both encrypt), + `hkdf = "0.12"` (workspace). Widen `hmac` to unconditional. |

**No new Cargo deps beyond `aes-gcm` + `hkdf`.** No `aes-siv` crate — design §7.2 builds the deterministic mode on top of `aes-gcm` + `hmac::Hmac<Sha256>`; the original "RFC 5297 AES-SIV" framing is rejected in favour of the design's actual construction.

---

## 1. File structure

```
crates/plugin-db/src/
  backend/
    mod.rs                              (+2 traits, +EncryptionMode, +SnapshotOpts/Handle, +PitrTarget, +4 BackendHandle accessors)
    postgres.rs                         (+2 impl blocks)
    sqlite/
      mod.rs                            (+2 impl blocks delegating to sub-modules)
      encryption.rs              (NEW)  SQLite-side EncryptedColumn glue
      backup.rs                  (NEW)  VACUUM INTO + restore=swap
      dialect.rs                        (+ encrypted arm)
      error.rs                          (+ BUSY → backup_busy)
  encryption/
    mod.rs                     (NEW)    public surface: encrypt(), decrypt(), EncryptionMode, KeyId
    aead.rs                    (NEW)    AeadKey + randomised/deterministic AEAD
    keys.rs                    (NEW)    HKDF derivation + KeyStore
    aad.rs                     (NEW)    canonical_aad(collection, column, row_pk?)
    wire.rs                    (NEW)    pack/unpack 12-byte nonce ‖ ct ‖ 16-byte tag
  query.rs                              (+ encryption-aware filter lowering; +column DDL)
  diff.rs                               (+ encryption: Option<EncryptionMeta>)
  crud/
    encryption_pass.rs         (NEW)    encrypt_row_on_write / decrypt_row_on_read
  crud.rs                               (call hooks in dispatch_insert/update/find)
  error.rs                              (+ column_key_not_configured, pitr_pg_only, encryption_aead_failed, ...)

sdks/db/src/
  types.ts                              (+ t.encrypted, EncryptedFieldOpts, FieldDef.encrypted)
  validate.ts                           (+ wrapped-type validation)
  collection.ts                         (+ validateEncryptedFieldsInFilter)
  errors.ts                             (+ new error codes)
```

---

## 2. Trait declarations

```rust
// In backend/mod.rs

pub trait EncryptedColumn: 'static {
    type KeyHandle: 'static;

    #[allow(async_fn_in_trait)]
    async fn resolve_key(&self, app_id: &str, key_id: &str)
        -> Result<Self::KeyHandle, DbError>;

    fn encrypt(&self, key: &Self::KeyHandle, mode: EncryptionMode,
               plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError>;

    fn decrypt(&self, key: &Self::KeyHandle, mode: EncryptionMode,
               ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    /// Per-row random nonce. AAD = (collection, column, row_pk_bytes) — binds
    /// ciphertext to its row position so an attacker with UPDATE access can't
    /// shuffle ciphertext between rows (the "ciphertext oracle" attack on
    /// randomised mode). Works single-phase in plugin-db because typed_id
    /// PKs are minted SDK-side BEFORE the INSERT (§13). No equality search.
    /// Default (fail-safe).
    Randomised,
    /// Synthetic nonce = HMAC-SHA256(k_siv, plaintext)[..12].
    /// Same plaintext → same ciphertext under (collection, column).
    /// AAD = (collection, column) only — row_pk not bound because
    /// deterministic mode's defining property is "same plaintext → same
    /// ciphertext across rows", which is incompatible with per-row AAD.
    /// Supports equality lookups via B-tree on ciphertext bytes.
    Deterministic,
}

pub trait Backup: 'static {
    #[allow(async_fn_in_trait)]
    async fn snapshot(&self, app_id: &str, dest_uri: &str, opts: SnapshotOpts)
        -> Result<SnapshotHandle, DbError>;

    #[allow(async_fn_in_trait)]
    async fn restore(&self, app_id: &str, snapshot: &SnapshotHandle)
        -> Result<(), DbError>;

    #[allow(async_fn_in_trait)]
    async fn pitr_replay(&self, app_id: &str, target: PitrTarget)
        -> Result<(), DbError>;
}

pub struct SnapshotOpts { pub if_busy: BusyPolicy }
pub enum BusyPolicy { Abort, Retry }
pub struct SnapshotHandle { pub uri: String, pub content_hash: [u8;32], pub created_at_ms: u64 }
pub enum PitrTarget { Lsn(String), TimeMillis(u64) }
```

---

## 3. `encryption/` module (cross-backend crypto)

### `aead.rs`

```rust
pub struct AeadKey { pub k_enc: [u8; 32], pub k_siv: [u8; 32] }

pub fn encrypt_randomised(key: &AeadKey, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
    // nonce = OsRng::fill_bytes(12)
    // ct||tag = AES_256_GCM.encrypt(k_enc, nonce, plaintext, aad)
    // wire::pack(nonce, ct, tag)
}

pub fn encrypt_deterministic(key: &AeadKey, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
    // nonce = HMAC_SHA256(k_siv, plaintext)[..12]
    // ct||tag = AES_256_GCM.encrypt(k_enc, nonce, plaintext, aad)
    // wire::pack(nonce, ct, tag)
}

pub fn decrypt(key: &AeadKey, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError>;
```

**AES-256** chosen over AES-128 — workspace default (`crates/core/Cargo.toml` already declares `aes-gcm = "0.10"`); ~5% CPU cost acceptable for an at-rest feature.

### `keys.rs`

KeyStore caches `(app_id, key_id) → AeadKey`. Two sources:
- **SQLite**: `ZEROSHIP_COLUMN_KEY_<KEYID>` env var (hex 32 bytes).
- **PG**: `__zeroship_admin.column_keys` table via SECURITY DEFINER getter (gated to `hardening`).

Both feed into HKDF: `salt = app_id`, `info = "zsenc/aead/v1/k_enc"` (or `_siv`). Result: `(k_enc, k_siv)` per app, per platform root. Salt-by-app closes cross-tenant ciphertext replay.

### `aad.rs`

```rust
pub fn canonical_aad(collection: &str, column: &str, row_pk_bytes: Option<&[u8]>) -> Vec<u8> {
    // Length-prefixed concat (big-endian u32 lengths) so (collection="ab", column="c")
    // cannot collide with (collection="a", column="bc"). row_pk_bytes is Some() in
    // Randomised, None in Deterministic.
}
```

### `wire.rs`

Layout: `[12 nonce | n ciphertext | 16 GCM tag]`. No sentinel prefix per design §7.2.

---

## 4. PG impl

### 4.1 `impl EncryptedColumn for PostgresBackend`

Thin delegator to `crate::encryption::*`. Key sourcing reads `__zeroship_admin.column_keys` via SECURITY DEFINER (gated under `hardening`); falls back to env-var sourcing in default builds for dev parity.

**Column DDL**: `BYTEA` for both modes. Deterministic gets an additional plain B-tree index on the column (`IndexKind::BTree` reused; no new kind).

### 4.2 `impl Backup for PostgresBackend`

- `snapshot`: spawn `pg_dump --schema="<app>" --format=custom --file=<temp>` via `compio::process::Command`. Upload to BlobStore. SHA-256 streamed. Returns `SnapshotHandle`.
- `restore`: download blob → temp file → `pg_restore` against a temp schema → `__zeroship_admin.swap_schema_atomic('<app>', '<app>__restoring')`. Holds the per-app register_model lock for the duration.
- `pitr_replay`: records target in `__zeroship_admin.pitr_targets`; returns `Ok(())`. **Operator runs the actual `recovery.conf` step.** P5 ships the API surface; full automation is P6+.

---

## 5. SQLite impl

### 5.1 `impl EncryptedColumn for SqliteBackend`

Delegator to `crate::encryption::*`. `KeyHandle = AeadKey`. `resolve_key` reads `ZEROSHIP_COLUMN_KEY_<KEYID>` env var (mirrors session-minter pattern).

**Column DDL**: `BLOB`. Deterministic gets `CREATE INDEX IF NOT EXISTS "<coll>__enc_<col>" ON "<coll>" ("<col>")` — SQLite handles BLOB index keys natively.

**Schema introspection**: regex on `sqlite_master.sql` for sentinel comment `/* zsenc:{mode}:{keyId}:{wraps} */` attached to the column's CHECK clause. Same regex-on-DDL pattern P4 uses for vector dims; sidecar `__zs_schema_meta` is the upgrade path.

### 5.2 `impl Backup for SqliteBackend` (`backend/sqlite/backup.rs`)

- `snapshot`: send `VacuumInto { dest_path }` to writer-actor. Actor runs `VACUUM INTO 'path'`. VACUUM INTO holds a shared-snapshot READ tx on the source; writers proceed concurrently against the WAL (design §16.1 round-4 fix). On `SQLITE_BUSY` (rare — only on schema-change race or checkpointer): `opts.if_busy = Abort` → `DbError::Coded { code: "backup_busy", retryable: true }`; `Retry` → 3 × 1s backoff.
- `restore`: download to `<db_dir>/zs-<app>.sqlite.restoring`; signal sibling workers to evict isolate; atomically rename to live file (POSIX rename atomic same-filesystem); next read re-ATTACHes.
- `pitr_replay`: `DbError::Configuration { code: "pitr_pg_only" }`.

**`vacuum_into_snapshot_consistent_under_concurrent_writer`** (CRITICAL #3 fence): source-side opens shared-snapshot read transaction; concurrent writers append past the snapshot's read mark; destination matches point-in-time of last commit visible when VACUUM INTO began. **No writer reservation taken on source.**

---

## 6. Schema DSL — SDK additions

```typescript
export type EncryptionMode = "randomised" | "deterministic";

export interface EncryptedFieldOpts {
  mode?: EncryptionMode;      // default "randomised" (fail-safe)
  keyId?: string;             // default "default"
  wraps?: TypeBuilder<string> | TypeBuilder<number> | TypeBuilder<Uint8Array>;
}

encrypted<T extends "string" | "number" | "bytes" = "string">(
  opts?: EncryptedFieldOpts
): TypeBuilder<InferWrapped<T>> {
  // Validates wraps ∈ {string, number, bytes}; arbitrary JSON deferred.
  return new TypeBuilder({
    type: opts?.wraps?._def.type ?? "string",  // wrapped primitive
    encrypted: {
      mode: opts?.mode ?? "randomised",
      keyId: opts?.keyId ?? "default",
      wraps: opts?.wraps?._def.type ?? "string",
    },
  });
}
```

**FieldDef** gains:
```typescript
encrypted?: { mode: "randomised" | "deterministic"; keyId: string; wraps: "string" | "number" | "bytes" };
```

Critical: `_def.type` is the **wrapped** primitive (not `"encrypted"`). DDL emitter checks `def.encrypted` FIRST and overrides type → `BYTEA`/`BLOB`. Validation walks the wrapped type as usual.

---

## 7. SDK filter validation (IMPORTANT #1 fence)

In `sdks/db/src/collection.ts`. Called at the top of every method accepting `filter` (`find`, `findOne`, `count`, `updateMany`, `deleteMany`, `distinct`, `findOrCreate`, `update`, `delete`, `exists`).

```typescript
function validateEncryptedFieldsInFilter(filter: PlainObject, schema: NormalizedSchema): void {
  for (const [key, value] of Object.entries(filter)) {
    if (key.startsWith("$")) { /* recurse $or/$and */ continue; }
    const def = schema[key];
    if (!def?.encrypted) continue;

    if (def.encrypted.mode === "randomised") {
      throw Object.assign(new Error(
        `Cannot filter on randomised-encrypted column "${key}". ` +
        `Switch to t.encrypted({ mode: "deterministic", ... }) if equality search is required.`,
      ), { code: "randomised_encrypted_field_not_filterable" as const });
    }
    // Deterministic: equality and $in only; reject $gt/$lt/$regex/etc.
    // ... see blueprint inline
  }
}
```

---

## 8. CRUD encryption pass (`crud/encryption_pass.rs`)

Transparent encrypt-on-write / decrypt-on-read pass. Runs inside existing CRUD dispatchers.

```rust
pub async fn encrypt_row_on_write<B: EncryptedColumn>(
    backend: &B, app_id: &str, collection: &str,
    schema: &serde_json::Value,
    row_pk: &str,                 // typed_id, always available — see §13
    row: &mut serde_json::Value,
) -> Result<(), DbError>;
```

**Row PK is always available** at this hook point because plugin-db mints typed_id PKs **SDK-side** (`crates/core/src/typed_id.rs::new(prefix)`) BEFORE the insert leaves the runtime — the `db.users.insert({...})` JS call populates `row.id` before RPC dispatch. The Rust crud layer receives the row with `id` already set; encryption sees the PK; single-phase INSERT works. This is the architectural property that makes P5 a Camp A design (see §13).

**AAD construction per column**:
```rust
let aad = canonical_aad(
    collection,
    column,
    match field.encryption.mode {
        Randomised    => Some(row_pk.as_bytes()),
        Deterministic => None,  // by design: same plaintext → same ct across rows
    },
);
```

**Call sites** in `crud.rs`:
- `dispatch_insert`: after `validate_row` (which populates `row.id` if missing) and before `build_insert`. `row_pk = row["id"].as_str()`.
- `dispatch_update`: same hook on partial-update object. The PK is in scope (it's the WHERE clause target). For UPDATEs of randomised-encrypted columns where the value changes, the new ciphertext uses the SAME PK in AAD — the row hasn't moved.
- `dispatch_find`: `decrypt_row_on_read` on every row after `rows_to_json_value`. The returned row carries `id`; AAD is reconstructed for decrypt verification.

**Wire format**: ciphertext → base64 in JSON `Value` (Value cannot carry raw bytes). The bind layer in `query.rs` recognises encrypted columns and base64-decodes back to BYTEA/BLOB at the parameter site.

---

## 9. Commit sequence — 6 PRs

### PR 1 — Trait surface + crypto module + workspace deps
- Declare `EncryptedColumn` + `Backup` traits + supporting types in `backend/mod.rs`.
- New `crates/plugin-db/src/encryption/` module (4 files).
- Workspace `Cargo.toml`: promote `aes-gcm = "0.10"`; add `hkdf = "0.12"`.
- Plugin-db `Cargo.toml`: add deps unconditional; widen `hmac`.
- `BackendHandle::as_encrypted_column_pg/_sqlite` + `as_backup_pg/_sqlite` accessors.
- Both backend `impl` blocks stub (return `p5_pr2_stub`).
- `ColumnInfo.encryption: Option<EncryptionMeta>` (default None).
- Gate: workspace builds clean; existing tests unchanged. Unit tests on encryption/* for round-trip + AAD-binding (tampered AAD → decrypt fails).

### PR 2 — PG `EncryptedColumn` + SDK `t.encrypted()` + CRUD pass
- `impl EncryptedColumn for PostgresBackend` (real body).
- `__zeroship_admin.column_keys` table + `get_column_key()` SECURITY DEFINER (under `hardening`).
- SDK `t.encrypted(opts?)` + `FieldDef.encrypted`.
- SDK `validateEncryptedFieldsInFilter` + wire into all filter methods.
- Rust `crud/encryption_pass.rs` + insertion points in `crud.rs`.
- PG dialect: encrypted DDL → `BYTEA`.
- `query.rs::build_create_indexes`: deterministic-mode encrypted columns get `IndexKind::BTree`.
- Gate: `encrypted_column_round_trip` (PG), `deterministic_encrypted_equality_via_index` (PG, CRITICAL #1 fence), `randomised_encrypted_full_scan_rejected_at_sdk` (SDK TS test, IMPORTANT #1 fence).

### PR 3 — SQLite `EncryptedColumn`
- `impl EncryptedColumn for SqliteBackend` (delegate; env-var key sourcing).
- `backend/sqlite/encryption.rs` (thin glue).
- SQLite dialect: encrypted → `BLOB` + sentinel comment for introspection.
- Schema introspection: regex on `sqlite_master.sql`.
- Gate: `encrypted_column_round_trip` (SQLite), `deterministic_encrypted_equality_via_index` (SQLite), cross-backend ciphertext-decryption equivalence test.

### PR 4 — PG `Backup`
- `impl Backup for PostgresBackend`.
- `pg_dump`/`pg_restore` shell-out via `compio::process::Command`.
- BlobStore integration (`snapshots/<app>/<ts>-<hash>.dump`).
- PITR placeholder: records target in `__zeroship_admin.pitr_targets`; returns `Ok(())`.
- Gate: `snapshot_restore_round_trip` (PG), `backup_in_progress_blocks_register_model`.

### PR 5 — SQLite `Backup`
- `impl Backup for SqliteBackend`.
- `backend/sqlite/backup.rs`: `VACUUM INTO` + `rusqlite::backup::Backup` fallback.
- Restore: download + atomic rename + isolate evict signal.
- `pitr_replay` → typed `Configuration { code: "pitr_pg_only" }`.
- Gate: `snapshot_restore_round_trip` (SQLite), `vacuum_into_snapshot_consistent_under_concurrent_writer` (CRITICAL #3 fence), `pitr_pg_only_returns_configuration_on_sqlite`.

### PR 6 — SDK polish + docs + design amendment
- `docs/reference/db.md`: Encryption section (modes, key sourcing, filter restrictions, operational caveats).
- `docs/reference/db-encryption-rotation.md` (stub pointing at P6b).
- `docs/runbooks/backup-restore.md`: operator runbook for both backends.
- Design amendment: §19 P5 → COMPLETE.
- Cross-backend filter-rejection equivalence test.
- **P5 COMPLETE**.

---

## 10. Critical details

- **AEAD**: AES-256-GCM via RustCrypto. Workspace dep `aes-gcm = "0.10"` already present in `crates/core/Cargo.toml`.
- **No `aes-siv` crate**: design §7.2 uses HMAC-derived synthetic nonce + AES-GCM construction, not RFC 5297 AES-SIV.
- **HKDF**: `hkdf = "0.12"` (RustCrypto). Per-app derivation: `salt = app_id`, `info = "zsenc/aead/v1/{slot}"`.
- **Key storage**: PG uses `__zeroship_admin.column_keys` (SECURITY DEFINER getter, hardening-gated). SQLite uses `ZEROSHIP_COLUMN_KEY_<KEYID>` env var. Both fall back to env-var sourcing in default PG builds for dev parity.
- **First-write check**: missing key → `Configuration { code: "column_key_not_configured" }` with `openssl rand -hex 32` hint.
- **CDC interaction**: encrypted columns surface as ciphertext bytes in `ChangeEvent`. Broker `publish()` passes bytes through; subscribers decrypt with their own key access. No plaintext leak through CDC.
- **Deterministic mode + CDC**: subscriber observing deterministic ciphertext can correlate equal plaintexts — same leak deterministic mode already accepts at the column-store layer.
- **Cache invalidation**: P5 doesn't need any (rotation deferred to P6b). KeyStore cache cleared on backend Drop.
- **Backup at-rest**: snapshot contains the *already-encrypted* ciphertext (the BYTEA/BLOB bytes). To decrypt the snapshot you still need the column key. Operators treat snapshots as sensitive but not key-equivalent.

---

## 11. Test gates (per design §19 P5)

| Gate | Backend | Shape |
| --- | --- | --- |
| `encrypted_column_round_trip` | PG, SQLite | Insert plaintext; read back; assert equal. |
| `deterministic_encrypted_equality_via_index` | PG, SQLite | 100 rows w/ deterministic-encrypted `ssn`; `find({ssn:"X"})`; assert matching set + EXPLAIN shows index hit. |
| `randomised_encrypted_full_scan_rejected_at_sdk` | TS unit | `find({ssnRandom:"X"})` throws `randomised_encrypted_field_not_filterable`; never reaches Rust. |
| `snapshot_restore_round_trip` | PG, SQLite | Insert N; snapshot; truncate; restore; assert row set + ciphertext bytes equal. |
| `vacuum_into_snapshot_consistent_under_concurrent_writer` | SQLite | Concurrent writer thread; trigger `VACUUM INTO`; assert snapshot ≤ live; assert live grows during snapshot. |
| `pitr_pg_only_returns_configuration_on_sqlite` | SQLite | `pitr_replay(...)` returns `Configuration { code: "pitr_pg_only" }`. |

**Bonus tests**:
- `encrypted_column_aad_tampering_rejected`: swap ciphertext between two columns; expect `encryption_aead_failed`.
- `randomised_ciphertext_row_swap_rejected` (NEW — P5 §13 fence): insert row A and row B in same column; swap their ciphertexts via raw UPDATE; read both; expect `encryption_aead_failed` on the swapped rows (the ciphertext-oracle defence on Randomised mode).
- `deterministic_ciphertext_row_swap_silently_succeeds`: same swap on a deterministic column; decryption succeeds because deterministic AAD intentionally omits row_pk; documents the known property.
- `cross_backend_ciphertext_decrypt`: encrypt on PG with key K; decrypt on SQLite with env var carrying same K; assert equal plaintext.

---

## 12. Open questions

| # | Question | P5 default |
| --- | --- | --- |
| Q-P5-A | **Riskiest** — row-PK in AAD? | **Yes for Randomised, No for Deterministic** (resolved 2026-05-24; see §13). Plugin-db is Camp A architecturally (typed_id PKs minted SDK-side); cost is zero extra round-trips. |
| Q-P5-B | `wraps` types beyond string/number/bytes? | **No** in P5; arbitrary JSON deferred. |
| Q-P5-C | AES key size: 128 vs 256? | **256**. Workspace default; ~5% CPU cost acceptable. |
| Q-P5-D | Key-id namespace: per-app or per-platform? | **Per-platform root + per-app HKDF derivation** (salt = app_id). |
| Q-P5-E | Backup file format/extension on SQLite? | `.sqlite` for VACUUM INTO; wrapped in BlobStore `snapshots/<app>/<ts>-<hash>.sqlite`. |
| Q-P5-F | PITR placeholder shape on PG? | `Ok(())` after recording target in `__zeroship_admin.pitr_targets`; operator runs `recovery.conf`. |
| Q-P5-G | Should `Backup` join `Backend` super-trait? | **No** — admin-surface; accessor-routed (mirrors `ChangeStream`/`VectorIndex`). |
| Q-P5-H | `t.encrypted()` on a `unique` field? | **Allowed for deterministic; refused for randomised.** |
| Q-P5-I | `t.encrypted()` on a `t.ref()` (FK) column? | **Refused at schema-definition time** with `encrypted_on_ref_unsupported`. |
| Q-P5-J | Decrypt failures on read: fail whole query or partial? | **Fail whole query** with `encryption_aead_failed`. |
| Q-P5-K | Probe key presence at startup or lazy? | **Lazy** — mirrors session-minter precedent. |

---

## 13. Riskiest decision — RESOLVED 2026-05-24

**Q-P5-A — AAD shape: include row PK for randomised mode (Camp A architecture).**

### Resolution

**P5 ships with AAD = `(collection, column, row_pk_bytes)` for `EncryptionMode::Randomised`** and AAD = `(collection, column)` for `EncryptionMode::Deterministic`. Single-phase INSERT in both cases. No two-phase write, no `RETURNING id` chicken-and-egg.

### Why this works in plugin-db (when it wouldn't work elsewhere)

The original objection to PK-in-AAD across the industry is the chicken-and-egg on INSERT: encryption needs the PK, but the DB generates the PK *during* INSERT. Microsoft Always Encrypted and MongoDB CSFLE both document this gap because they're driver-layer encryption — they see a partial row before the server fills in the auto-generated PK.

**Plugin-db doesn't have that constraint.** Per `AGENTS.md` "Key invariants":

> typed_id everywhere. UUIDv7 + base62 + entity prefix (`usr_…`, `app_…`, `ses_…`). Defined in `crates/core/src/typed_id.rs`.

Every primary key in every plugin-db table is a typed_id minted **SDK-side** before the row reaches the wire. `db.users.insert({...})` populates `row.id` in JavaScript via `typed_id.new("usr")`; by the time the Rust crud layer sees the row, `id` is set. The encryption pass can fold those PK bytes into AAD with zero extra round-trips.

This puts plugin-db architecturally in **Camp A** alongside:
- **AWS DynamoDB Encryption Client** — `AAD = (table, partition_key, sort_key)`. AWS controls the full stack; keys are supplied by the caller before put-item.
- **AWS KMS Encryption Context** — official guidance recommends `{table, id, purpose}`-style AAD.
- **AWS Encryption SDK** — same.
- **Google Cloud Tink AEAD** — docs explicitly: "bind to a context with per-record identifier".
- **HashiCorp Vault Transit** — docs: "use a unique context per record for per-record binding".

And explicitly NOT in **Camp B** with:
- **Microsoft Always Encrypted** — driver-layer encryption; docs explicitly warn "does not protect against unauthorized data movement within a column".
- **MongoDB CSFLE** — driver-layer; same gap, same warning.

The Camp B systems document the gap because their architecture can't avoid it. Plugin-db's architecture can, so it would be negligent not to.

### What this protects against

| Attack | With PK-in-AAD (P5) | Without PK-in-AAD |
|---|---|---|
| **Ciphertext oracle on randomised columns** (attacker with UPDATE access copies ciphertext from row A to row B they control, reads row B through the app to reveal A's plaintext) | **Blocked** — AAD mismatch → `encryption_aead_failed` on read | Silent plaintext leak |
| **Row contamination** (attacker swaps SSNs between users by shuffling ciphertexts) | **Detected** as `encryption_aead_failed` | Silent — wrong PII served to wrong user |
| **Backup tamper** (attacker edits backup, swaps row positions) | **Detected** on first read after restore | Silent corruption |
| **Cross-row rollback** (replay old ciphertext into a different row position) | **Blocked** | Silent |

The ciphertext-oracle attack is the most concrete win: it converts "UPDATE-only access" (which today reads as "less serious than read access") into a real plaintext-leak vector. PK-in-AAD blocks it.

### Cost (single-phase INSERT preserved)

```rust
// crud::dispatch_insert (sketch):
let row_pk = row["id"].as_str().expect("typed_id always set by SDK");
for col, def in schema:
    if let Some(enc_meta) = def.encrypted:
        let aad = canonical_aad(collection, col, match enc_meta.mode {
            Randomised    => Some(row_pk.as_bytes()),
            Deterministic => None,
        });
        let ct = backend.encrypt(key, enc_meta.mode, plaintext, &aad);
        row[col] = base64(ct);
// Single INSERT with row[id] + ciphertexts goes out as ONE round-trip.
```

Zero additional round-trips vs the current INSERT path. The only added work is one `canonical_aad()` call per encrypted column at write time, which is sub-microsecond pure-Rust bytewise concat.

### Why deterministic mode skips row_pk

Deterministic encryption's defining property: same plaintext → same ciphertext under `(collection, column)`. That property is what makes the B-tree index on ciphertext work for equality lookups — it's the whole point of the mode. If we folded `row_pk` into deterministic-mode AAD, every row would produce a different ciphertext even for equal plaintexts, breaking the index lookup. Deterministic mode therefore keeps `AAD = (collection, column)` and inherits the standard deterministic-mode leak (equality across rows is observable). The SDK refuses range / `LIKE` queries on deterministic columns regardless.

### UPDATE semantics

When a randomised-encrypted column is UPDATEd, the new ciphertext uses the **same** row_pk in AAD — the row didn't move. The PK is in the WHERE clause; it's trivially in scope. UPDATE-PK (changing the primary key value) is not a supported operation in plugin-db's schema DSL — typed_id PKs are immutable post-INSERT, so this whole class of "PK migration breaks AAD" worry doesn't apply.

### What's still deferred

- **Key rotation** stays in P6b. With or without PK-in-AAD, the rotation mechanics are the same.
- **Two-phase INSERT** code path stays not-implemented — it would be needed only if we ever added DB-side-generated PK support (`SERIAL`/`IDENTITY`), which the platform doesn't expose and isn't planned.
- **`EncryptionMode::RandomisedBoundToRow` as a sibling variant** — no longer needed. Randomised is bound to row by default.

### Reviewer sign-off

This reverses the prior recommendation in this section (which was: "omit row-PK; reviewer sign-off required for the stricter option"). The corrected analysis lands on the stricter option as the *cheaper* one given plugin-db's actual architecture. **No reviewer sign-off needed** — this is the Camp-A-aligned default, matches AWS DynamoDB Encryption Client / Tink / AWS KMS public guidance, and costs zero extra round-trips.
