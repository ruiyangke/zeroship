> Archived 2026-05-25: shipped. Live reference: docs/reference/db.md.

# Sensitive Field Masking — Never-Decrypt-Automatically

**Status**: design proposal.
**Lands**: between P5 (encryption baseline) and P6 (hardening). Modifies P5's read semantics — see §10 migration.
**Affects**: SDK `Row<S>` types, every read-path crud method, every encrypted column declaration, the V8 ↔ Rust boundary for column values, the platform's CREATE TABLE emission (adds a sibling masked column per masked field).
**Replaces**: P5's "transparent decrypt on read" default.

**Storage strategy resolved 2026-05-24 — Path B (dedicated masked column).** The masked representation is **pre-computed at write time and stored as a sibling column** alongside the ciphertext. Default reads `SELECT <col>_masked` (no decrypt, no key access); unmask `SELECT <col>` (ciphertext) + decrypt + audit. See §9 for the implementation, §13 for the trade-off analysis (Path A — compute on read — was rejected in favour of Path B's key-scope reduction + PCI-3.4 alignment + DB-layer role-separation enablement).

---

## 1. The pivot

**P5 as currently implemented**: encrypted columns are decrypted server-side on every read and returned as plaintext to V8.

**This proposal**: encrypted columns are **never decrypted automatically**. Reads serve a pre-computed masked representation from a sibling column; obtaining the plaintext requires an explicit `.unmask(...)` call that:

1. Round-trips to the Rust crud layer.
2. Verifies the calling actor's authorization against the column's classification.
3. Emits an audit row (`__zeroship_audit_unmask`).
4. SELECTs the ciphertext column, decrypts, and returns plaintext to V8 if (and only if) authorization passed.

This is the **"safe by default, explicit reveal"** model. Inspired by:
- **Tink's** explicit `Aead.decrypt()` call (no transparent magic).
- **AWS Secrets Manager**'s `GetSecretValue` audit boundary.
- **HashiCorp Vault**'s lease-based reveal pattern.
- **Salesforce Shield**'s field-level audit on decryption.
- **Stripe**'s storage of `card.last4` alongside the encrypted PAN.

The motivation: zeroship hosts AI-generated app code. The default behaviour where `console.log(user)` prints plaintext SSNs is structurally unsafe — the platform cannot rely on AI-generated code to remember to redact. Moving the default to "masked unless explicitly unmasked" closes this leak class cryptographically — the plaintext **never crosses the V8 boundary** unless the app code asks for it via an audited call.

**Why dedicated columns (Path B)** instead of computing the mask on read (Path A):

1. **Key-access scope reduction** — Path A requires the worker process to hold the column key in memory for every read (to decrypt → compute mask). Path B holds the key ONLY during the rare `unmask` RPC. A worker not currently handling an unmask call has no key material in scope.
2. **PCI DSS 3.4 alignment** — "Render PAN unreadable anywhere it is stored." The masked column IS the rendered representation, stored as such. A PCI auditor can inspect the schema directly.
3. **DB-layer role separation** — `GRANT SELECT (id, name, ssn_masked) ON users` to one PG role; `SELECT (id, name, ssn) ON users` to another. Pure PG primitive; the DB enforces the distinction without platform-code involvement.
4. **Audit signal sharpens** — only unmask events touch the decrypt path; every audit row corresponds to a real reveal, not a noisy "served a masked read" event.
5. **Backup/restore inspection** — operators can spot-check snapshots via the masked column without holding the key.

---

## 2. Read semantics — the new default

### Before (P5 current)

```typescript
const user = await db.users.findOne({ id: "usr_xyz" });
console.log(user.ssn);  // "123-45-6789" — plaintext, automatic decrypt
```

### After (this proposal)

```typescript
const user = await db.users.findOne({ id: "usr_xyz" });
console.log(user.ssn);  // "***-**-6789" — MaskedValue.toString() yields the masked repr

// To get plaintext, an explicit call is required:
const plaintext = await user.ssn.unmask({ reason: "user requested view" });
// → if authorized: returns "123-45-6789", writes audit row
// → if not authorized: throws { code: "unmask_not_permitted" }
```

The plaintext does **not** sit in the row object. `user.ssn` is a `MaskedValue<string>` instance whose `.toString()` / `JSON.stringify()` / template-literal interpolation all yield the masked form. Plaintext is fetched on demand via a separate RPC.

---

## 3. Schema declaration

Masking is declarable on every column type, not just encrypted ones. Encrypted columns get a default mask if none is specified.

```typescript
users: {
  // Encrypted column. No explicit mask → default mask kind = "full" ("***")
  ssn: t.encrypted({ mode: "randomised" }),

  // Encrypted + explicit last-4 mask
  card_pan: t.encrypted({ mode: "deterministic" }).mask({ kind: "last4" }),

  // Plaintext-at-rest column, masked at egress (e.g., support agent's view)
  email: t.string().mask({ kind: "email" }),

  // Plaintext-at-rest, full mask (visible only to admins)
  internal_notes: t.string().mask({ kind: "full" }),

  // Plain column — no mask, no encryption
  display_name: t.string(),
}
```

### Default-mask-on-encryption rule

If a column is declared `t.encrypted(...)` **without** an explicit `.mask(...)`, the platform applies `mask({ kind: "full" })` automatically. Rationale: encryption signals "this is sensitive" and the absence of an explicit mask should not be interpreted as "show plaintext". Fail-safe default.

If a creator genuinely wants encrypted-at-rest but plaintext-on-read (e.g., a column read only by authenticated background jobs that already operate at the trust boundary), they declare `.mask({ kind: "none" })` — explicit opt-out.

### Built-in mask strategies

| Kind | Input | Output | Use |
|---|---|---|---|
| `"full"` | anything | `"***"` (or `<MASKED>`) | maximum redaction; default for encrypted |
| `"last4"` | `"123-45-6789"` | `"***-**-6789"` | SSN, card numbers, phone numbers |
| `"first4"` | `"4111-1111-1111-1234"` | `"4111-****-****-****"` | BIN/IIN preservation for card networks |
| `"email"` | `"alice@example.com"` | `"a****@example.com"` | preserve domain for sorting/filtering analytics |
| `"name"` | `"Alice Anderson"` | `"A. A***"` | initials |
| `"date-year"` | `"1985-04-12"` | `"1985-**-**"` | preserve year for age buckets |
| `"date-decade"` | `"1985-04-12"` | `"198?-**-**"` | even coarser |
| `"none"` | anything | passthrough (full plaintext) | explicit opt-out — requires acknowledgement that this column is leaky-by-design |
| `{ kind: "builtin", name: "redact_ipv4" }` | `"192.168.1.10"` | `"192.168.x.x"` | named platform-built-in only |

**No raw user-defined JS functions for masking.** Creator-supplied mask functions are a security risk (an AI-generated `mask: (v) => v` defeats the purpose). Only named built-in strategies. Adding a new strategy is a platform PR, not creator config.

---

## 4. SDK type system

### `MaskedValue<T>` wrapper

```typescript
class MaskedValue<T extends string | number | Uint8Array> {
  /** The masked representation. Safe to log, serialize, render. */
  readonly masked: string;

  /** The classification of the source field, for app-side policy checks. */
  readonly classification: "public" | "pii" | "spi" | "phi" | "pci" | "internal";

  /**
   * Round-trip to the platform to fetch plaintext.
   * - Verifies the calling actor's role allows unmasking this classification.
   * - Emits an audit row.
   * - Returns plaintext or throws { code: "unmask_not_permitted" }.
   */
  unmask(opts: {
    actor?: Actor;        // defaults to current request's actor
    reason?: string;       // free-text reason for audit (recommended)
  }): Promise<T>;

  /** Check authorization WITHOUT triggering the audit row. */
  canUnmask(opts: { actor?: Actor }): Promise<boolean>;

  /** Implicit string coercion → masked repr. */
  toString(): string;          // returns this.masked
  toJSON(): string;            // returns this.masked
  [Symbol.toPrimitive](): string;  // returns this.masked
}
```

### `Row<S>` type inference

```typescript
// Schema:
users: {
  ssn:   t.encrypted({ mode: "randomised" }),
  email: t.string().mask({ kind: "email" }),
  name:  t.string(),  // unmasked
}

// Inferred Row<S>:
{
  id:    string,                       // system field, no mask
  ssn:   MaskedValue<string>,           // encrypted → default "full" mask
  email: MaskedValue<string>,           // explicit "email" mask
  name:  string,                        // no mask
  // ... system fields (created_at, etc.)
}
```

Concretely: TypeScript's mapped types pick up `FieldDef.mask` presence and wrap the field type in `MaskedValue<...>` at the type level. Unmasked fields stay as their wrapped primitive type. This means the type system flags `user.ssn === "123"` at compile time (`MaskedValue<string>` doesn't compare equal to `string`).

### Bulk unmask

```typescript
// Unmask multiple fields in one round-trip:
const reveal = await user.unmask(["ssn", "email"], { reason: "admin view" });
// → returns { ssn: "123-45-6789", email: "alice@example.com" } if all authorized
// → throws on the FIRST unauthorized column (atomic; no partial reveals)

// Sub-set:
const safe = await user.unmask(["email"]);  // ssn stays masked
```

### Per-query unmask hint (advanced)

```typescript
// Tell the read path to return plaintext directly, skipping the mask wrapper.
// This is the closest equivalent to current P5 behavior — for high-trust paths
// like worker-to-worker background jobs.
const user = await db.users.findOne({ id }, {
  unmask: ["ssn"],  // platform check happens at find time
  actor: serviceAccount,
});
// user.ssn is `string`, not `MaskedValue<string>`
```

Same authorization check; same audit row. Just the round-trip is folded into the original read instead of being a follow-up.

---

## 5. Authorization policy

### Three layers

1. **Schema declares classification.** The encrypted column or `.mask()` modifier carries a classification.
2. **Platform-config declares actor-role → classification mapping.** Per-app config, not per-call.
3. **Caller's actor role drives authorization.** From the P3 SessionMinter token, the actor and role are known.

### Platform-config shape

```typescript
// In the app's bootstrap (sdks/bootstrap or a dedicated config file):
import { defineMaskPolicy } from "@zeroship/db";

defineMaskPolicy({
  admin:        ["public", "pii", "spi", "phi", "pci", "internal"],  // sees all
  support:      ["public", "pii"],                                    // PII only (not PHI/PCI)
  user:         ["public"],                                           // public only
  ai_assistant: [],                                                   // can unmask nothing
  // The system "auto" actor (background workers, migrations) defaults to ALL —
  // configurable via `auto: [...]` if you want to restrict it.
});
```

Stored as a JSON blob in `__zeroship_admin.mask_policies` (PG) or a sidecar file (SQLite dev). One policy per app; applied to every unmask call from that app.

If no policy is defined, the safe default is: **only the `auto` actor kind can unmask any classification; all other actors see masked**. This protects new apps from accidentally exposing data — the creator has to explicitly opt-in to revealing.

### Default classifications

| Classification | Examples |
|---|---|
| `"public"` | usernames, display names, public profile data |
| `"pii"` | full name, email, address, phone, IP address, date of birth |
| `"spi"` | SSN, driver's license, biometric data (CPRA "sensitive PI") |
| `"phi"` | health records, medical IDs, diagnosis (HIPAA-scope) |
| `"pci"` | card numbers, CVV, magnetic stripe data (PCI-scope) |
| `"internal"` | platform-internal metadata, system field overrides |

If `t.encrypted()` is declared without classification, defaults to `"pii"`. Explicit always wins.

---

## 6. Wire flow

### Storage shape (Path B)

For every masked column declared in the schema, the platform emits **two physical columns**:

```sql
-- Creator declares:
--   users: { ssn: t.encrypted({ mode: "randomised" }).mask({ kind: "last4" }) }
-- Platform emits:
CREATE TABLE "app_xyz"."users" (
  id          TEXT PRIMARY KEY,
  ssn         BYTEA NOT NULL,    -- ciphertext (the P5 wire format: 0x01||nonce||ct||tag)
  ssn_masked  TEXT NOT NULL,      -- pre-computed mask: "***-**-6789"
  ...
);
CREATE INDEX "users__ssn_masked_idx" ON "app_xyz"."users" ("ssn_masked");
-- ↑ optional index; enables analytics queries on the masked representation
--   ("show users whose card ends in 1234" pattern). Auto-emitted only when the
--   column is also marked .index() or .uniqueIndex(); otherwise omitted.
```

Naming convention: the sibling column is `<col>_masked` (TEXT for string/number masks, BYTEA for bytes-mask). Reserved by `validate_field_name` — creators cannot declare `<col>_masked` themselves when `<col>` carries a `.mask()` modifier.

### Write (INSERT / UPDATE)

```
JS:    db.users.insert({ ssn: "123-45-6789", ... })
        │
        ▼  RPC dispatch
        │
Rust:  crud::dispatch_insert
        ├─ validate_row → row.id minted via typed_id
        │
        ├─ crud::encryption_pass::encrypt_row_on_write(...):
        │    for each column with encryption metadata:
        │      ① resolve_key(app_id, key_id) → AeadKey
        │      ② aad = canonical_aad(coll, col, Some(row_pk_bytes))  // Camp A
        │      ③ ciphertext = encrypt(key, plaintext, aad)
        │      ④ row[col] = base64(ciphertext)
        │
        ├─ crud::mask_pass::apply_mask_on_write(...):       ← NEW (PR 2)
        │    for each column with mask metadata:
        │      ⑤ masked = mask_transform(plaintext, kind)   // "123-45-6789" → "***-**-6789"
        │      ⑥ row["ssn_masked"] = masked
        │
        ├─ build_insert sees TWO columns to bind: ssn (BYTEA) + ssn_masked (TEXT)
        ├─ Single INSERT INTO users (id, ssn, ssn_masked, ...) VALUES ($1, $2, $3, ...);
        │   ↑ atomic per-row write of both columns
        │
        ▼   row inserted with both ciphertext + masked stored
```

The mask transform consumes plaintext, NOT ciphertext. Plaintext is in scope during the encryption pass; the mask pass runs alongside, then both columns are bound in a single INSERT.

### Default read (no unmask)

```
JS:    db.users.findOne({ id: "usr_xyz" })
        │
        ▼  RPC dispatch
        │
Rust:  crud::dispatch_find_one
        ├─ build_select_for_read:                                 ← MODIFIED (PR 2)
        │    for each column in schema:
        │      • non-masked column → SELECT "col"
        │      • masked column     → SELECT "col_masked" (NOT "col")
        │   So the SELECT NEVER touches the ciphertext column.
        │   Generated SQL:
        │      SELECT id, ssn_masked AS ssn, name, ... FROM users WHERE id = $1
        │      ↑ alias rename so the row's JSON shape uses the schema-declared name
        │
        ├─ run query → ssn = "***-**-6789" (plain TEXT bytes from PG)
        │
        ├─ crud::mask_pass::wrap_row_on_read(...):                 ← NEW (PR 2)
        │    for each column with mask metadata:
        │      row[col] = MaskedValueRepr {
        │        masked: "***-**-6789",
        │        classification: "spi",
        │      }
        │
        ▼   row returned to V8
        │
JS:     user.ssn instanceof MaskedValue
        user.ssn.toString() === "***-**-6789"
        // ✓ No decryption happened.
        // ✓ The column key was never resolved.
        // ✓ Plaintext never crossed the V8 boundary.
        // ✓ The ciphertext column wasn't even READ from disk.
```

The wrap step is trivial (just attaches metadata); no key access, no decryption.

### Explicit unmask

```
JS:    await user.ssn.unmask({ reason: "admin view" })
        │
        ▼  Native op: env.db.unmaskField {
        │        collection: "users",
        │        row_pk: "usr_xyz",
        │        column: "ssn",
        │        actor: <session.actor_id>,
        │        reason: "admin view",
        │     }
        │
Rust:  crud::dispatch_unmask
        ├─ Authorization check (from policy):
        │    classification = "spi"
        │    actor.role = "admin"
        │    policy["admin"] includes "spi" → ALLOWED
        │    (denied path: write audit row with outcome="denied", return Forbidden)
        │
        ├─ Now (and only now) resolve_key + load ciphertext:
        │    SELECT "ssn" FROM users WHERE id = $1     ← ciphertext fetched HERE
        │    plaintext = decrypt(key, ciphertext, aad) ← key loaded into scope
        │
        ├─ Emit audit row:
        │    __zeroship_audit_unmask {
        │      ts, actor_id, app_id, collection, row_pk, column,
        │      classification, reason, request_id, outcome: "granted"
        │    }
        │
        ▼   return plaintext to V8
        │
JS:     const ssn = await user.ssn.unmask(...)
        ssn === "123-45-6789"
        // Plaintext is now in V8 — app code is responsible from here.
```

Key observation: the key was loaded ONLY for this unmask call. Default reads never load it.

### Per-query unmask hint

```
JS:    db.users.findOne({ id }, { unmask: ["ssn"], actor })
        │
        ▼  RPC dispatch with unmask hint
        │
Rust:  crud::dispatch_find_one
        ├─ Authorization check upfront (before query):
        │    for col in unmask list: verify actor.role can unmask col's classification
        │    if any fails → return Forbidden BEFORE running query
        │
        ├─ build_select_for_read with unmask hint:
        │    SELECT id, ssn, ssn_masked AS __ssn_masked, name, ... FROM users WHERE id = $1
        │    ↑ pull BOTH columns; we'll decrypt ssn and return plaintext, ignore masked
        │
        ├─ decrypt ssn → plaintext; row[col] = plaintext (NOT MaskedValueRepr)
        ├─ emit audit row per unmasked column
        │
        ▼   return row to V8 with mixed shape:
        │      { id: string, ssn: string (plaintext), email: MaskedValue }
        │
JS:     user.ssn === "123-45-6789"
        user.email instanceof MaskedValue
```

---

## 7. Audit trail

Every unmask call writes a row to `__zeroship_audit_unmask`:

```sql
CREATE TABLE IF NOT EXISTS "<app>"."__zeroship_audit_unmask" (
    id              BIGSERIAL PRIMARY KEY,
    ts              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    actor_id        TEXT NULL,            -- session.actor_id from P3
    actor_role      TEXT NULL,            -- role at time of call
    collection      TEXT NOT NULL,
    row_pk          TEXT NOT NULL,
    column          TEXT NOT NULL,
    classification  TEXT NOT NULL,        -- "pii", "spi", "phi", etc.
    reason          TEXT NULL,            -- free-text from caller
    request_id      TEXT NULL,            -- propagated from gateway
    outcome         TEXT NOT NULL         -- "granted" | "denied"
);

CREATE INDEX ON "<app>"."__zeroship_audit_unmask" (ts);
CREATE INDEX ON "<app>"."__zeroship_audit_unmask" (actor_id, ts);
CREATE INDEX ON "<app>"."__zeroship_audit_unmask" (row_pk, column, ts);
```

**Both granted and denied attempts are logged**. Denied attempts are operationally important (potential exfil attempt; misconfigured app).

Retention: 6 years by default (HIPAA requirement). Configurable per app via `defineMaskPolicy({ retention: "7y" })`. Background job (P8+) purges rows past retention.

---

## 8. Composition with other features

### With encryption (P5) — dual-column atomic writes

The encryption pass and the mask pass run **side by side at INSERT/UPDATE**. Both write to the same row in a single SQL statement:

- Encryption pass: `row["ssn"] = base64(encrypt(plaintext))`
- Mask pass: `row["ssn_masked"] = mask_transform(plaintext, kind)`
- Single `INSERT INTO users (id, ssn, ssn_masked, ...) VALUES ($1, $2, $3, ...);`

Both columns are bound in one statement → atomic per-row. **Drift prevention** lives at this layer: there's no code path that updates one without the other.

For UPDATE: when the encrypted column's plaintext changes, BOTH columns rewrite. When the plaintext doesn't change (e.g., updating a sibling column), neither rewrites.

Default reads `SELECT ssn_masked AS ssn FROM users` — the ciphertext column is not touched. Unmask reads `SELECT ssn FROM users` — the masked column is not touched.

### With deterministic-mode encryption

`db.users.find({ email_hash: "alice@..." })` still works the same way as P5:
- Deterministic mode: the filter value is encrypted client-side using the deterministic AEAD; the B-tree index on the `email_hash` ciphertext column hits.
- Returned rows have `email_hash` as `MaskedValue<string>` by default; explicit `.unmask()` reveals.

The filter targets the **ciphertext** column (where the B-tree index lives); the read returns the **masked** column. Two separate columns; both accessed within a single SELECT (`SELECT email_hash_masked AS email_hash, ... WHERE email_hash = $1::bytea`).

### With CDC (P2)

`ChangeEvent` carries ciphertext for encrypted columns (per P5; not plaintext). With dedicated masked columns, CDC events naturally include **both** columns in the row image — subscribers see ciphertext + masked, never plaintext. This matches the "no automatic plaintext escape" rule:

- Default CDC subscriber receives the masked representation as the user-visible value.
- A subscriber that needs plaintext calls `.unmask()` on the value (which round-trips back to the platform with their actor's credentials, hits the same authorization + audit path).

The masked column makes CDC subscribers SIMPLER — they don't need to know they're seeing a masked value; they just see the schema-declared name with safe content.

### With backup/restore (P5 PR 4-5)

Snapshot files carry **both** columns: `ssn` (ciphertext) AND `ssn_masked` (plaintext). The masked column survives backup/restore round-trips unchanged. Operators inspecting a backup file can spot-check `ssn_masked` content without holding the key — useful for compliance audits and incident triage.

Restore reinstalls both columns; mask metadata is re-derived from schema (not stored per row beyond the column itself).

### With schema introspection (`diff.rs`)

`ColumnInfo` already has `encryption: Option<EncryptionMeta>` (P5 PR 1). Mask metadata folds into it:

```rust
pub struct ColumnInfo {
    // ... existing ...
    pub encryption: Option<EncryptionMeta>,
    pub mask: Option<MaskMeta>,           // ← NEW
}

pub struct MaskMeta {
    pub kind: MaskKind,
    pub classification: Classification,
    pub sibling_column: String,           // e.g., "ssn_masked"
}
```

The schema diff classifier treats `mask: Some` as additive (adding a sibling column + computing initial values is `Recoverable` change class). Removing a mask is destructive (drops the sibling column, can't be rolled back without re-encryption).

### With aggregations

```typescript
// Works — count doesn't reveal:
await db.users.count({ active: true });

// Works — group by deterministic-encrypted column:
await db.users.aggregate([{ $group: { _id: "$email_hash" } }]);
// → returns groups keyed by MaskedValue (or by ciphertext bytes, depending on impl;
//   probably ciphertext bytes for grouping semantics)

// Refused — server-side SUM on encrypted is meaningless:
await db.users.aggregate([{ $group: { _id: null, total: { $sum: "$salary" } } }]);
// → throws if salary is encrypted (existing P5 refusal; no change)
```

---

## 9. Implementation considerations

### Where the dual write/read happens

Rust crud layer. New module `crud/mask_pass.rs` for the transform; SQL builder modifications in `query.rs` for the SELECT/INSERT/UPDATE column-list rewriting (alias the masked column back to the schema-declared name; bind the ciphertext column under its raw name).

### Sibling-column visibility — hide from the SDK surface

The `<col>_masked` column is a **platform implementation detail**, not part of the creator-facing API. Specifically:

- Schema introspection on the creator's end (e.g., type generation in `sdks/vite-plugin`) sees only the original `ssn` field, NOT a `ssn_masked` sibling.
- `Row<S>` type inference produces `{ ssn: MaskedValue<string> }`, NOT `{ ssn: MaskedValue<string>, ssn_masked: string }`.
- Filter expressions like `db.users.find({ ssn_masked: "..." })` are refused with `reserved_field_name`-style error.
- The SDK's response transform aliases `ssn_masked` back to `ssn` (via SQL `AS` clause in the SELECT) so the JS-side value comes through under the user-declared field name.
- Schema diff classifier excludes the `_masked` siblings from the "what changed" output — these columns are derived; their presence/absence is implicit from the parent column's `.mask()` modifier.

In other words: the masked column is **how** the platform stores the masked representation, not a thing the creator can declare, query, or see. From the JS side, the only knob is the parent column's `.mask({...})` declaration; everything else is invisible.

This keeps the SDK API surface identical to the Path A version (`user.ssn` is a `MaskedValue<string>`; `.unmask()` for plaintext). The only thing that changes is **where the mask comes from at read time** — Path B reads from disk; Path A computes it. From the creator's perspective the difference is invisible.

The auto-emitted `users__ssn_masked_idx` index is similarly internal — visible only via introspection of the live PG schema, never surfaced through the SDK schema introspection API. This is OK because indexes are operational metadata, not part of the schema contract.

### Cache for unmask round-trips

A naive `unmask` re-fetches the row's ciphertext column and re-decrypts. For a UI that lists 50 users and the admin clicks "reveal SSN" on one, that's a single round-trip — fine.

But if the admin clicks "reveal all", we'd do 50 round-trips. Two mitigations:

1. **Bulk unmask**: `await db.users.bulkUnmask([{id: "usr_a", columns: ["ssn"]}, ...])` — one round-trip, returns a map.
2. **Per-request plaintext cache**: hold decrypted plaintexts in a `RefCell<HashMap<(coll, pk, col), String>>` keyed by request. Cleared at request end. Subsequent `.unmask()` calls in the same request hit the cache instead of re-decrypting.

Recommend (1) for the common pattern; (2) as an internal performance optimisation.

### Drift detection (background job)

Even with atomic dual-writes, drift between ciphertext and masked column is theoretically possible via:
- Direct SQL by an operator (DBA bypasses the platform write path).
- Bug in the CRUD pass that updates one without the other.
- Backup/restore mishap.

A periodic drift-detection cron job samples N random rows per app per masked column:
1. Decrypt the ciphertext column.
2. Recompute the mask transform.
3. Compare to the stored masked column.
4. Alarm on mismatch (`tracing::error!` + audit row with `outcome: "drift_detected"`).

Frequency: weekly per app, 1% sample rate. Costs negligible. Catches drift quickly without re-encrypting every row.

### Per-app config storage

`defineMaskPolicy({...})` writes to `__zeroship_admin.mask_policies` (PG) or `<db_dir>/mask_policies.json` (SQLite). Read at app boot; cached in `IsolateDbContext::mask_policy`.

Hot-reload semantics: changing a policy requires app restart (acceptable for v1).

### Authorization check

Same SECURITY DEFINER pattern as P3 sessions. The unmask RPC:
1. Validates the session token (P3).
2. Reads the actor's role from the token.
3. Reads the mask policy for the app.
4. Checks `policy[actor.role]` includes the column's classification.
5. If yes, proceeds with decrypt+return; if no, returns `Forbidden { code: "unmask_not_permitted" }` and writes a `denied` audit row.

### Performance

Mask transforms are cheap (string slice + concat). Per-row overhead is microseconds. The expensive part is the audit row write — one INSERT per unmask. Batched in the CRUD flow when bulk-unmask is used.

For non-masked columns: zero overhead. Masking only kicks in when `FieldDef.mask` is present.

---

## 10. Migration story (the breaking change)

P5 PRs 1-5 currently implement "transparent decrypt on read". Apps that have already used `t.encrypted(...)` expect to see plaintext on read.

This proposal changes the default. Three options:

**(A) Hard switch with version flag.**
- Bump the SDK major version to v2.
- v2 apps see masked-by-default.
- v1 apps see plaintext-by-default until they migrate.
- The platform's SDK version checker enforces.

**(B) Per-column opt-in for the new behaviour.**
- Existing `t.encrypted()` keeps "decrypt on read" (no mask).
- New `t.encrypted({...}).mask({...})` triggers the new behaviour.
- The "default mask if encrypted" rule from §3 applies only to NEW columns added post-deployment.

**(C) Soft switch with deprecation window.**
- All `t.encrypted(...)` columns ship the new behaviour immediately.
- Apps that were relying on transparent decrypt get a `MaskedValue` instead of plaintext.
- They break loudly (TS type mismatch at compile time; `.unmask()` runtime call at runtime).
- The migration is mechanical: every `user.ssn` becomes `await user.ssn.unmask({...})`.
- The deprecation window is one minor release cycle.

**Recommendation: (C)**.

Reasons:
1. P5 is fresh — there are NO production apps relying on transparent decrypt yet.
2. The TS type wrapper catches mismatches at compile time; AI-generated apps will compile-fail on update if they were doing the wrong thing.
3. Soft switch + loud break = creators learn the new model immediately; no silent semantic drift.

**Migration content**: a migration guide at `docs/reference/migration/p5-to-masked-decrypt.md` that walks through:
- The change.
- Every encrypted column gets a default `.mask({ kind: "full" })` if not specified.
- Apps need to either:
  - Call `.unmask()` in places that previously assumed plaintext.
  - OR opt out with explicit `.mask({ kind: "none" })`.
- Platform tooling to scan an app's source code for `user.encrypted_field` patterns and flag them as candidates for the migration.

---

## 11. Commit sequence — 8 PRs (Path B; sibling-column-based)

All 6 must-have capabilities (sibling-column emission, alias SELECT, backfill, rewrite-on-mask-change, drift detection, reserved-name validation) are mandatory deliverables — none optional. Path B is incomplete without the full set; partial implementations risk drift or surface leaks.

### PR 1 — Schema DSL + `MaskedValue` types + reserved-name validator + sibling-column metadata

- `sdks/db/src/types.ts`: `t.string().mask(opts)` / `t.encrypted(opts).mask(opts)`; `MaskedValue<T>` class with `.toString()` / `.toJSON()` / `.unmask()` shape.
- `FieldDef.mask: { kind: MaskKind; classification?: Classification }`.
- TypeScript inference: `Row<S>` automatically wraps masked fields in `MaskedValue<T>`. **The `<col>_masked` sibling is NEVER part of `Row<S>` — invisible to creator code (§9).**
- Built-in mask kinds: "full", "last4", "first4", "email", "name", "date-year", "date-decade", "none". Built-in classifications: "public" / "pii" / "spi" / "phi" / "pci" / "internal".
- Default mask = `"full"` + classification = `"pii"` when `t.encrypted()` declared without explicit mask.
- **Reserved-name validator** (`validate_field_name`): refuse creator-defined fields ending in `_masked` (whether or not a parent column exists with that prefix). Reserve the `_masked` suffix globally on encrypted apps. Reserve all six default classifications as reserved column names (`pii`, `spi`, `phi`, `pci`, `internal`, `public`). Both fence the namespace.
- `crates/plugin-db/src/diff.rs::ColumnInfo`: add `mask: Option<MaskMeta>` field alongside the existing `encryption` field. `MaskMeta` carries `{ kind, classification, sibling_column: String }`.

### PR 2 — DDL emission for sibling column + Path B INSERT/UPDATE rewrite

- `query.rs::build_create_table_with_fks`: when a column has `def.mask = Some(_)`, emit TWO physical columns: the parent (BYTEA if encrypted, TEXT/INT/BYTEA otherwise) + the sibling `<col>_masked` (TEXT or BYTEA depending on mask kind).
- Auto-emit `CREATE INDEX <coll>__<col>_masked_idx ON <coll>(<col>_masked)` ONLY when the parent column is declared with `.index()` or `.uniqueIndex()`. Avoid auto-indexing every masked column — would balloon storage on collections with many masked columns.
- New `crates/plugin-db/src/crud/mask_pass.rs`:
  - `apply_mask_on_write(schema, row)`: walks schema, for every field with `mask`, computes the masked representation from the plaintext value and writes `row["<col>_masked"]`.
  - 8 mask transforms (one per `MaskKind`).
- `query.rs::build_insert` + `build_update_one`: bind BOTH columns. Single-row atomic write.
- Bypass `_masked` from SDK-visible schema introspection — the SDK's introspection layer (`@zeroship/db` schema reflection) walks `FieldDef`s, not the live PG schema, so the sibling columns never appear in creator-visible output.

### PR 3 — Aliased SELECT for default reads + `MaskedValue` wire shape

- `query.rs::build_select_for_read`: when a column has `def.mask = Some(_)`, the SELECT clause emits `<col>_masked AS <col>` (alias back to schema-declared name). The ciphertext column is **NOT** touched in the default read path.
- New `crud::mask_pass::wrap_row_on_read`: attach `MaskedValueRepr { masked, classification }` metadata to each masked field in the returned row.
- V8 wrapper materialises `MaskedValueRepr` → `MaskedValue<T>` instance with `.unmask()` method.
- `dispatch_find/find_one/find_one_returning` route through the new SELECT shape.
- Tests: assert default read SQL contains `<col>_masked AS <col>` and NOT `<col>` for masked fields.

### PR 4 — Unmask RPC + authorization + audit (`__zeroship_audit_unmask`)

- New native op `env.db.unmaskField { collection, row_pk, column, actor, reason }`.
- New `crud::dispatch_unmask`:
  1. Authorization check against policy.
  2. SELECT ciphertext column for the target row.
  3. Decrypt under the column key.
  4. Write audit row (granted OR denied; both logged).
  5. Return plaintext if granted; `Forbidden { code: "unmask_not_permitted" }` if denied.
- `auth/bootstrap.rs`: emit `__zeroship_audit_unmask` table (PG) per the schema in §7. SQLite equivalent: sidecar table in the per-app SQLite file (same schema).
- SDK `MaskedValue.unmask({ actor?, reason? })` method routes to the new RPC.

### PR 5 — `defineMaskPolicy()` SDK + platform-config storage

- `sdks/db/src/policy.ts`: `defineMaskPolicy({ admin: [...], support: [...], user: [...] })`.
- Storage:
  - PG: `__zeroship_admin.mask_policies` table (gated `hardening`); one row per app, JSON blob.
  - SQLite: sidecar JSON file `<db_dir>/mask_policies.json`.
- Read at `IsolateDbContext` boot; cached.
- Authorization helper in Rust: `MaskPolicy::can_unmask(actor_role, classification) -> bool`.
- Default policy when none declared: only `auto` actor kind can unmask anything.

### PR 6 — Backfill on first-mask-declaration + rewrite on mask-kind-change

This PR makes mask metadata changes safe to apply to existing data. **Two paths**:

**6a — First-time mask declaration on an existing column**:
- Schema diff detects: column EXISTS, but `mask: None` → `mask: Some(...)`.
- Classification: `Additive` for the sibling column add; `Recoverable` for the backfill operation.
- DDL: `ALTER TABLE <coll> ADD COLUMN <col>_masked TEXT/BYTEA NULL` (nullable so the ALTER doesn't fail on existing rows).
- Backfill job (background; uses the existing `__zeroship_migrations` audit table):
  1. SELECT all rows with `<col>_masked IS NULL`.
  2. For each row: decrypt `<col>` ciphertext → compute mask → UPDATE `<col>_masked`.
  3. Track progress in `__zeroship_migrations`.
  4. On completion: `ALTER TABLE <coll> ALTER COLUMN <col>_masked SET NOT NULL`.

**6b — Mask-kind change**:
- Schema diff detects: column has `mask: Some(old_kind)` → `mask: Some(new_kind)`.
- Classification: `Recoverable` (can roll back to old kind by re-rewriting).
- Rewrite job (background): SELECT all rows; for each, decrypt ciphertext → compute mask with NEW kind → UPDATE `<col>_masked` with new value.
- Track in `__zeroship_migrations`; resumable on worker restart.

**6c — Mask removal** (`t.encrypted()` → `t.encrypted()` without `.mask()`, or `.mask({ kind: "none" })`):
- Schema diff detects mask removal.
- Classification: `Destructive` (data loss from the operator's perspective; the sibling column gets dropped).
- Under `strictness="strict"`: refuse; require operator opt-in via the existing P0 deploy strictness mechanism.
- Under `strictness="lenient"` or `"off"`: drop the `<col>_masked` column.

### PR 7 — Drift detection cron + bulk unmask + per-query unmask hint

- **Drift detection** (the must-have):
  - New background job `crud::mask_drift::run_drift_check_for_app(app_id)`.
  - Schedule: weekly per app, 1% row sample per masked column.
  - For each sampled row: decrypt ciphertext → compute mask with current kind → compare to stored `<col>_masked` column.
  - On mismatch: write audit row with `outcome: "drift_detected"`, `tracing::error!`, increment a per-app drift counter; future P6+ surface to a dashboard.
  - The cron lives in the existing maintenance-cron infrastructure (whichever pattern the F1 sweeper-half uses in P6a — share the scheduler).
- **Bulk unmask**: SDK `user.unmask(["ssn", "email"])` and `db.users.bulkUnmask([{id, columns}, ...])`. Single RPC; authorization atomic (first unauthorized fails the whole call).
- **Per-query unmask hint**: `db.users.findOne({ id }, { unmask: ["ssn"], actor })`. Authorization upfront before query; row carries plaintext for unmask-listed columns, `MaskedValue` for the rest.

### PR 8 — Migration tooling + creator-facing docs + design amendment → P5.5 COMPLETE

- `zeroship migrate scan-mask-usage` CLI: walks the creator's source for patterns that would have worked under P5 transparent-decrypt (`user.encrypted_field` as string) and flags them as candidates for `.unmask()`.
- `docs/reference/migration/p5-to-masked-decrypt.md`: migration walkthrough.
- `docs/reference/db.md`: Masking section + worked examples.
- `docs/proposals/db-system-design.md`: amendment block dated 2026-05-24 recording the read-semantic flip + Path B sibling-column strategy.
- Closeout test gates (from §12 + new):
  - `encrypted_column_default_mask_full`
  - `mask_last4_redacts_correctly`
  - `unmask_with_authorized_actor_returns_plaintext`
  - `unmask_with_unauthorized_actor_returns_forbidden_audit_logged`
  - `unmask_writes_audit_row_with_correct_classification`
  - `bulk_unmask_atomic_first_unauthorized_fails_all`
  - `cdc_event_carries_masked_value_for_masked_columns`
  - `per_query_unmask_hint_works`
  - `mask_policy_per_app_isolated`
  - `default_read_does_not_touch_ciphertext_column` (Path B fence: assert the SQL EXPLAIN doesn't include the ciphertext column)
  - `default_read_does_not_load_column_key` (Path B fence: assert the key-store cache lookup count is zero on default reads; non-zero on unmask reads)
  - `sibling_masked_column_not_visible_in_sdk_introspection` (the hide-from-SDK invariant)
  - `creator_cannot_query_by_masked_sibling` (validate_field_name refuses `_masked` suffix in filter clauses)
  - `mask_kind_change_rewrites_sibling_column` (PR 6b path)
  - `mask_addition_backfills_existing_rows` (PR 6a path)
  - `drift_detection_catches_diverged_sibling` (PR 7 cron path)
- Snapshot tests for SDK type inference (`Row<S>` shape with `MaskedValue` fields; `_masked` sibling absent).

---

## 12. Open questions

| # | Question | Default |
|---|---|---|
| Q-MASK-A | Default mask for encrypted columns: `"full"` or `"last4"`? | `"full"` — safest; creators opt for less restrictive explicitly. |
| Q-MASK-B | Should `.mask({ kind: "none" })` require an attestation string? E.g., `.mask({ kind: "none", attestation: "I understand this column is leaky" })`. | No — keep it terse. `kind: "none"` is itself the attestation. Docs explain the risk. |
| Q-MASK-C | Audit row on every read (granted or only on denied)? | Both. Denied is the security signal; granted is the compliance signal. |
| Q-MASK-D | Auth check at find-time or at unmask-time? | At unmask-time (lazy). Find returns masked even if the actor IS authorized; explicit unmask is always the path. Per-query `{ unmask: [...] }` hint folds the check into find. |
| Q-MASK-E | Plaintext cache within a request — TTL or per-request? | Per-request only. Cleared at request end. |
| Q-MASK-F | Bulk unmask atomic-or-partial on failure? | Atomic. First unauthorized column fails the whole call. Caller can retry with the unauthorized columns omitted. |
| Q-MASK-G | CDC subscribers see plaintext for non-encrypted-but-masked columns? | No — CDC carries the masked representation. Subscribers `.unmask()` if needed. |
| Q-MASK-H | Should the mask policy be PER-APP (default) or PER-COLLECTION? | Per-app. Per-collection adds complexity; can be added later as a refinement (P9+). |
| Q-MASK-I | Default policy when `defineMaskPolicy()` is NOT called by the creator? | "Auto actor sees all; everyone else sees masked." Forces creators to think about this. |
| Q-MASK-J | Backwards compat for v1 apps (no mask declarations)? | All encrypted columns retroactively get `.mask({ kind: "full" })`. Creator can override with `.mask({ kind: "none" })`. |
| Q-MASK-K | TypeScript inference for `MaskedValue<T>` — can it be transparent (just T) when the actor has unmask permission? | No — actor permission is runtime; types are compile-time. Always `MaskedValue<T>`. Compile-time safety requires the unmask round-trip. |
| Q-MASK-L | Empty/null plaintext masking — what does `mask({ kind: "last4" })` do on `null`? | `null` passes through as `null` (no mask). Empty string → `""`. |
| Q-MASK-M | Are mask kinds extensible? Can a creator request a new built-in? | Yes via platform PR. The set is intentionally small. |

---

## 13. Riskiest decision

**Replacing P5's transparent decrypt-on-read with explicit unmask-only.**

This is a breaking change for any code (creator's or AI-generated) that currently assumes `user.encrypted_field` returns plaintext.

**Why we should do it anyway**:
1. P5 has not landed in production yet — only PR 1-4 are in flight, no live creator apps depend on the transparent behaviour.
2. The default-safe model genuinely fixes the AI-generated-code leak risk.
3. The TypeScript wrapper catches mismatches at compile time, not runtime.
4. Industry best practice: HashiCorp Vault, AWS Secrets Manager, Tink, Salesforce Shield all use explicit-reveal models for sensitive data; none transparent-decrypt.

**The honest cost**: every creator-facing example in `docs/reference/db.md` that shows reading encrypted fields needs updating. The mental model shifts from "encryption is transparent" to "encryption is two-step: read masked, unmask when needed". That's a docs + onboarding adjustment.

**Mitigation**: PR 6's migration guide + the `zeroship migrate scan-mask-usage` tool make the upgrade mechanical. The TypeScript compiler does the heavy lifting at app-rebuild time.

This decision needs explicit user sign-off before PR 1 dispatches. **The P5 read semantic shift is creator-visible and changes the SDK contract.** Per the pilot directive, this is exactly the kind of decision that requires check-in.

---

## Status: SHIPPED 2026-05-24

All 8 PRs landed (commit-only; not pushed per pilot directive). Test
counts as of PR 8 close: plugin-db `lib` 599 / `lib+hardening` ≥ 613
/ `lib+sqlite` ≥ 689 / `sqlite_integration` 85 / `zeroship` CLI 9.

| PR | Commit     | One-line summary                                                                  |
|----|------------|-----------------------------------------------------------------------------------|
| 1  | `49857c31` | Masking foundation: `MaskedValue<T>`, reserved `_masked` suffix, `ColumnInfo.mask`. |
| 2  | `d8e54269` | DDL sibling-column emission (`<col>_masked TEXT NOT NULL`) + dual-write CRUD pass.|
| 3  | `2e866360` | Default read flipped to masked: aliased SELECT + `MaskedValue<T>` rehydration.    |
| 4  | `9e9b9a62` | `unmaskField` RPC + `__zeroship_audit_unmask` table + authorization stub.         |
| 5  | `a6ed24d3` | `defineMaskPolicy()` + per-app policy storage + classification-based authorization.|
| 6  | `e22e0754` | Mask backfill (6a) + rewrite (6b) + removal (6c) under deploy strictness.         |
| 7  | `e08adb44` | Drift detection cron + bulk unmask + per-query `{ unmask: [...] }` hint.          |
| 8  | _this PR_  | `zeroship migrate scan-mask-usage` CLI + creator docs + §11 closeout gates → P5.5 COMPLETE. |

The amendment block in `docs/proposals/db-system-design.md`
(2026-05-24) records the cross-system perspective. Creator docs
land in `docs/reference/db.md` (Masking section) and
`docs/reference/migration/p5-to-masked-decrypt.md` (migration
walkthrough).

Deferred follow-ups recorded in the db-system-design amendment:
AAD version binding (→ P7.5 once `version` lands), P6+ drift
dashboard, per-collection mask policies (Q-MASK-H, → P9+).

## Amendment — P9 PR 2/PR 4 (2026-05-24): `MaskedValue` is a native v8_class; unmask + policy relocated

Two P9 (API/ABI alignment) changes touch this design:

- **`MaskedValue` is now a native `#[v8_class]`** (`crates/plugin-db/src/v8_classes/masked_value.rs`), minted directly by the row serializer (P9 PR 2). The old `{sentinel: "__zsmask__", ...}` JSON sentinel + the SDK's `mapResultDoc` JS rehydration loop are gone — Rust hands V8 a real `MaskedValue` instance on the first hop. Public surface (getters `masked`/`classification`/`_meta`; methods `unmask`/`canUnmask`/`toString`/`toJSON`/`[Symbol.toPrimitive]`) is byte-identical to the old TS class, so `Row<S>['ssn']`'s observable type is unchanged. The published `.d.ts` ships a hand-maintained `declare class MaskedValue<Value>` preserving the `_plaintext` phantom.

- **`unmaskField` / `bulkUnmaskFields` moved off `Db` to `Collection`** (P9 PR 2): `Collection.unmaskField(rowPk, col, opts)` + `Collection.bulkUnmask(items, opts)` (collection name inherited from the receiver, not spoofable) and `MaskedValue.unmask(...)` (dispatches from the instance's own bound `_meta`).

- **`setMaskPolicy` moved behind the `__platform` capability handle** (P9 PR 4): it is no longer a method on `env.db`. `defineMaskPolicy()` still works unchanged for creators; the boot-time flush (`runtime-entry.ts`) now calls `__platform.setMaskPolicy(...)` via the V8 private-symbol handle instead of `env.db.setMaskPolicy(...)`. The authorization path (`crud::unmask::check_unmask_authorization`) and the policy storage are unchanged.
