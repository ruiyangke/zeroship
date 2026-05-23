# Sensitive Field Masking — Never-Decrypt-Automatically

**Status**: design proposal.
**Lands**: between P5 (encryption baseline) and P6 (hardening). Modifies P5's read semantics — see §10 migration.
**Affects**: SDK `Row<S>` types, every read-path crud method, every encrypted column declaration, the V8 ↔ Rust boundary for column values.
**Replaces**: P5's "transparent decrypt on read" default.

---

## 1. The pivot

**P5 as currently implemented**: encrypted columns are decrypted server-side and returned as plaintext to V8.

**This proposal**: encrypted columns are **never decrypted automatically**. Reads return a `MaskedValue<T>` wrapper that carries the *masked representation* of the field; obtaining the plaintext requires an explicit `.unmask(...)` call that:

1. Round-trips to the Rust crud layer.
2. Verifies the calling actor's authorization against the column's classification.
3. Emits an audit row (`__zeroship_audit_unmask`).
4. Returns the plaintext to V8 if (and only if) authorization passed.

This is the **"safe by default, explicit reveal"** model. Inspired by:
- **Tink's** explicit `Aead.decrypt()` call (no transparent magic).
- **AWS Secrets Manager**'s `GetSecretValue` audit boundary.
- **HashiCorp Vault**'s lease-based reveal pattern.
- **Salesforce Shield**'s field-level audit on decryption.

The motivation: zeroship hosts AI-generated app code. The default behaviour where `console.log(user)` prints plaintext SSNs is structurally unsafe — the platform cannot rely on AI-generated code to remember to redact. Moving the default to "masked unless explicitly unmasked" closes this leak class cryptographically — the plaintext **never crosses the V8 boundary** unless the app code asks for it via an audited call.

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

### Read (no unmask)

```
JS:    db.users.findOne({ id: "usr_xyz" })
        │
        ▼  RPC dispatch
        │
Rust:  crud::dispatch_find_one
        ├─ build SELECT * FROM users WHERE id = $1
        ├─ run query → row with ciphertext bytes for ssn, plaintext for other cols
        │
        ├─ crud::mask_pass::apply_mask_on_read(...):
        │    for each column with mask metadata:
        │      ① if column is encrypted → decrypt (need plaintext to apply mask)
        │      ② apply mask transform (last4, full, email, ...)
        │      ③ replace row[col] = MaskedValueRepr {
        │            masked: "***-**-6789",
        │            classification: "spi",
        │            // server NEVER includes plaintext here
        │          }
        │
        ▼   row returned to V8: ssn is { masked, classification }
        │
JS:     user.ssn instanceof MaskedValue
        user.ssn.toString() === "***-**-6789"
        // Plaintext doesn't exist in V8 memory.
```

The Rust layer briefly held plaintext (step ①) to compute the mask, then discarded it. V8 never sees plaintext.

### Read + explicit unmask

```
JS:    await user.ssn.unmask({ reason: "admin view" })
        │
        ▼  RPC: zeroship.db.unmaskField {
        │        collection: "users",
        │        row_pk: "usr_xyz",
        │        column: "ssn",
        │        actor: <session.actor_id>,
        │        reason: "admin view",
        │     }
        │
Rust:  crud::dispatch_unmask
        ├─ Authorization check:
        │    classification = "spi" (from schema)
        │    actor.role = "admin"
        │    policy["admin"] includes "spi" → ALLOWED
        │
        ├─ Re-fetch the row (or use a short-lived plaintext cache — see §9)
        ├─ Decrypt (for encrypted columns)
        ├─ Emit audit row:
        │    __zeroship_audit_unmask {
        │      ts, actor_id, app_id, collection, row_pk, column,
        │      classification, reason, request_id
        │    }
        │
        ▼   return plaintext to V8
        │
JS:     const ssn = await user.ssn.unmask(...)
        ssn === "123-45-6789"
        // Plaintext is now in V8 — app code is responsible from here.
```

### Read + per-query unmask hint

```
JS:    db.users.findOne({ id }, { unmask: ["ssn"], actor })
        │
        ▼  RPC dispatch with unmask hint
        │
Rust:  crud::dispatch_find_one
        ├─ Authorization check upfront (before decrypt):
        │    for col in unmask list: verify actor.role can unmask col's classification
        │    if any fails → return Forbidden BEFORE running query
        │
        ├─ run query, decrypt, mask other cols, leave unmask-listed cols as plaintext
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

### With encryption (P5)

- Encrypted column without explicit mask → default mask "full".
- Encrypted column with explicit mask → mask is applied to decrypted plaintext.
- Unmask round-trip decrypts AGAIN (or uses cache — see §9) and returns plaintext.

### With deterministic-mode encryption

`db.users.find({ email_hash: "alice@..." })` still works:
- The filter value is plaintext (the SDK encrypts it deterministically before binding).
- The B-tree index hits.
- Returned rows have `email_hash` as `MaskedValue<string>` by default; explicit `.unmask()` reveals.

The filter-time encryption is unaffected by masking. Masking is purely about read-time presentation.

### With CDC (P2)

`ChangeEvent` carries ciphertext for encrypted columns (per P5; not plaintext). For non-encrypted-but-masked columns, the CDC event currently carries plaintext.

**Decision**: CDC events for masked columns carry **the masked representation**, not plaintext. Rationale: CDC subscribers are app code, subject to the same "no automatic plaintext escape" rule as direct reads. Subscribers that need plaintext call `.unmask()` on the value (which round-trips back to the platform with their actor).

### With backup/restore (P5 PR 4+5)

Snapshot files carry ciphertext for encrypted columns + plaintext for non-encrypted columns. Masking is not a storage-layer concern. After restore, the same mask metadata applies (mask is re-derived from schema, not stored per row).

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

### Where the mask is applied

Rust crud layer, right after `decrypt_row_on_read` and before the row returns to V8. New module `crud/mask_pass.rs`.

### Cache for unmask round-trips

A naive `unmask` re-fetches the row and re-decrypts. For a UI that lists 50 users and the admin clicks "reveal SSN" on one, that's a single round-trip — fine.

But if the admin clicks "reveal all", we'd do 50 round-trips. Two mitigations:

1. **Bulk unmask**: `await db.users.bulkUnmask([{id: "usr_a", columns: ["ssn"]}, ...])` — one round-trip, returns a map.
2. **Per-request plaintext cache**: hold decrypted plaintexts in a `RefCell<HashMap<(coll, pk, col), String>>` keyed by request. Cleared at request end. Subsequent `.unmask()` calls in the same request hit the cache instead of re-decrypting.

Recommend (1) for the common pattern; (2) as an internal performance optimisation.

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

## 11. Commit sequence — 7 PRs

### PR 1 — Schema DSL + `MaskedValue` types + mask kinds

- `sdks/db/src/types.ts`: `t.string().mask(opts)` / `t.encrypted(opts).mask(opts)`; `MaskedValue<T>` class.
- `FieldDef.mask: { kind: MaskKind; classification?: string }`.
- TypeScript inference: `Row<S>` automatically wraps masked fields in `MaskedValue<T>`.
- Built-in mask kinds: "full", "last4", "first4", "email", "name", "date-year", "date-decade", "none".
- Default mask = "full" when `t.encrypted()` declared without explicit mask.

### PR 2 — Rust mask transforms + crud mask pass

- `crates/plugin-db/src/crud/mask_pass.rs`: 8 mask transforms (one per kind) + classification metadata.
- Insertion point in `crud::dispatch_find/find_one/find_one_returning`: after `decrypt_row_on_read`, apply mask per column.
- Change wire format: returned row carries `{ masked: string, classification: string }` for masked columns instead of plaintext.
- **P5 PR 2/3.5's CRUD encryption pass is unchanged**; this PR layers ON TOP of it.

### PR 3 — `unmask()` RPC + authorization + audit

- New native op: `zeroship.db.unmaskField { collection, row_pk, column, actor, reason }`.
- New `dispatch_unmask` in `crud/mod.rs`.
- Authorization helper: load policy from `__zeroship_admin.mask_policies`, check `policy[actor.role]`.
- `__zeroship_audit_unmask` table in `auth/bootstrap.rs` (PG) / `<db_dir>/audit_unmask.sqlite` (SQLite).
- SDK `MaskedValue.unmask()` method.

### PR 4 — `defineMaskPolicy()` SDK + platform-config storage

- `sdks/db/src/policy.ts`: `defineMaskPolicy(opts)`.
- Rust-side: read at app bootstrap into `IsolateDbContext::mask_policy`.
- Storage: `__zeroship_admin.mask_policies` table (PG, gated `hardening`); sidecar JSON file on SQLite.

### PR 5 — Bulk unmask + per-query unmask hint

- SDK: `user.unmask([cols])`, `db.users.findOne({}, { unmask: [...] })`.
- Rust: extend `dispatch_find` to accept an `unmask_hint` parameter; pre-authorize, then decrypt+return plaintext for unmask-listed cols.

### PR 6 — Migration tooling + creator-facing docs

- `zeroship migrate scan-mask-usage` CLI subcommand: walks the app's source code, flags places where `user.encrypted_field` is read as plaintext (likely candidates for `.unmask()`).
- `docs/reference/migration/p5-to-masked-decrypt.md`.
- `docs/reference/db.md` Masking section.

### PR 7 — Test gates + polish

- Test gates per design §7 P5 + new mask tests:
  - `encrypted_column_default_mask_full`
  - `mask_last4_redacts_correctly`
  - `unmask_with_authorized_actor_returns_plaintext`
  - `unmask_with_unauthorized_actor_returns_forbidden_audit_logged`
  - `unmask_writes_audit_row_with_correct_classification`
  - `bulk_unmask_atomic_first_unauthorized_fails_all`
  - `cdc_event_carries_masked_value_for_masked_columns`
  - `per_query_unmask_hint_works`
  - `mask_policy_per_app_isolated` (cross-tenant: app A's policy doesn't affect app B)
- Snapshot tests for SDK type inference (`Row<S>` shape with masked fields).
- Closeout: design doc amendment + P5 PR 2/3/3.5 docstring updates noting the read semantic shift.

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
