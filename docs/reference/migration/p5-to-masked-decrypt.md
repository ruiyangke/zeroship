# Migrating from P5 transparent-decrypt to P5.5 masked-default

**Status**: shipped 2026-05-24 (P5.5 PRs 1-8).
**Affects**: every app that declares a `t.encrypted(...)` column.
**Breaking?**: Yes at the SDK-type level. See "Will my P5-era apps
break?" below for the pre-launch context.

---

## The behaviour flip

Under the pre-P5.5 model, reading an encrypted column transparently
returned the plaintext. A handler that wrote:

```ts
const { data: user } = await env.db.users.get(id);
console.log(user.ssn); // "123-45-6789"
```

…saw the SSN as a bare string. The runtime decrypted on every read.

Under P5.5 (PR 3), the default read returns a **`MaskedValue<string>`**
wrapping the pre-computed masked text:

```ts
const { data: user } = await env.db.users.get(id);
console.log(user.ssn); // MaskedValue { ssn: "***-**-6789" }
user.ssn.toString();   // "***-**-6789"
```

The bare string only appears after an explicit `.unmask()` round-trip
or an `unmask` hint on the query.

**Why**: AI-generated handlers often log / echo / return objects
wholesale. With transparent decrypt, every `console.log(user)` leaks
the entire row's plaintext into logs / response bodies / error traces.
The default-masked model treats plaintext as an explicit-reveal
operation — the request HAS to ask, and that ask is audited.

---

## How to recognise affected code

Run the migration scanner from the repo root:

```bash
zeroship migrate scan-mask-usage --path=./src
```

Flags every property access on a name that looks like an encrypted
column (heuristic: ends in a known PII suffix — `ssn`, `email`,
`phone`, `dob`, `creditCard`, etc.). Exit code is 0 if clean, 1 if
any findings; CI can wire it as a gate.

`--format=json` dumps a flat array suitable for piping into other
tools. Each finding carries the file, line, column, the matched
snippet, and a suggested replacement using `.unmask()` with
`actor` / `reason` placeholders the creator fills in.

**The scanner is best-effort.** It uses a regex-shaped heuristic, not
a full TypeScript parser. False positives (e.g. `device.phone` for a
non-encrypted field named `phone`) are expected; **every flagged spot
needs review**. False negatives are likely too — bracket access
(`row["ssn"]`), spread destructuring (`{ ssn, ...rest } = user`),
and JSON serialization-then-property-access aren't matched.

For airtight coverage, `tsc` is the second pass: the type system
flags every mismatch where a handler treats a `MaskedValue<T>` as a
`T`.

---

## Three migration patterns

### 1. Mask is fine — leave it

If the value is only ever displayed in the UI for the row's owner,
or fed into a place where the masked form is acceptable (an audit
log, a summary email), do nothing. The masked value flows
transparently:

```tsx
function UserCard({ user }: { user: Row<typeof Users> }) {
  return <div>SSN: {user.ssn.toString()}</div>;
  //                   ^ renders "***-**-6789"
}
```

`MaskedValue.toString()` and `MaskedValue.toJSON()` both return the
masked text. The most common case for `t.encrypted({...}).mask({ kind:
"last4" })` columns is "show the user the last 4 of their own card so
they recognise it" — no unmask needed.

### 2. Unmask on demand

When the handler genuinely needs the plaintext — to verify a security
question, to push to a downstream system, to format a tax document —
call `.unmask()`:

```ts
const { data: user } = await env.db.users.get(id);
const ssn = await user.ssn.unmask({
  actor: "support_agent",
  reason: "verify identity claim ticket-12345",
});
console.log(ssn); // "123-45-6789"
```

Every `.unmask()` writes a row to `__zeroship_audit_unmask`:
`{ app_id, collection, row_pk, column, classification, actor, reason,
outcome }` where outcome is `"granted"` or `"denied"`. Denied calls
throw `code: "unmask_not_permitted"`. The audit trail is the
compliance contract — `actor` and `reason` are required fields, the
SDK rejects empty strings.

For multiple columns on the same row, batch:

```ts
const [ssn, email] = await user.unmask(["ssn", "email"], {
  actor, reason,
});
```

Batch unmask is atomic — if any column refuses authorization, the
whole call fails and no plaintext is returned.

### 3. Per-query unmask hint

When you know upfront you'll need plaintext for a specific column,
pass `unmask` on the query options. The auth check runs once before
the SELECT, and the row carries plaintext for the hinted columns
(masked for the rest):

```ts
const { data: user } = await env.db.users
  .find(
    { id },
    { unmask: ["ssn"], actor: "tax_handler", reason: "1099 generation" } as never,
  )
  .first();
console.log(user.ssn); // "123-45-6789" (plaintext, not MaskedValue)
console.log(user.email); // MaskedValue { email: "f****@example.com" }
```

The hint changes the wire SELECT: the parent ciphertext column is
fetched and decrypted instead of the sibling. Useful when the
plaintext is needed for `> 1` row at once — `find({}, { unmask:
[...] } as never)` (awaited as an array) decrypts every returned row
in one pass.

---

## Worked example — tax form handler

### Before (P5 transparent-decrypt)

```ts
export const fetch = async (req: Request, env: Env) => {
  const { userId } = await req.json();
  const { data: user } = await env.db.users.get(userId);

  return Response.json({
    name: user.name,
    ssn:  user.ssn,     // ← plaintext
    dob:  user.dob,     // ← plaintext
    // Whole row leaks if a future maintainer adds: ...user
  });
};
```

### After (P5.5 masked-default + per-query hint)

```ts
export const fetch = async (req: Request, env: Env) => {
  const { userId } = await req.json();
  const actor = env.user.id;  // gateway-injected
  const { data: user } = await env.db.users
    .find(
      { id: userId },
      {
        unmask: ["ssn", "dob"],
        actor,
        reason: `1099 generation for user ${userId}`,
      } as never,
    )
    .first();
  if (!user) return new Response("not found", { status: 404 });

  return Response.json({
    name: user.name,
    ssn:  user.ssn,     // plaintext (hint applied)
    dob:  user.dob,     // plaintext (hint applied)
  });
};
```

Two changes:
1. Switch `get()` → `find(... , { unmask, actor, reason } as never).first()`.
   The hint authorizes upfront and decrypts before the row hits the
   handler.
2. Provide `actor` + `reason`. Both land in
   `__zeroship_audit_unmask`. `reason` should be a human-readable
   string a compliance reviewer can follow.

The `name` field stays unmasked because it isn't encrypted /
isn't masked; the rest of the row's column types are untouched by
the change.

---

## FAQ

### Why was this changed?

AI-generated handlers leak plaintext easily. The pre-P5.5 model
let `console.log(user)`, `Response.json(user)`, or
`throw new Error(JSON.stringify(user))` dump the entire row's
sensitive content into observable surfaces. The default-masked
model:

- Makes plaintext exposure **explicit** in code (a `.unmask()` call
  is visible in review and `tsc` types).
- Makes plaintext exposure **audited** (`__zeroship_audit_unmask`
  has a row per call).
- Makes the safe path **easy** (do nothing, get masked).
- Makes the dangerous path **annotated** (`actor` + `reason` are
  required, not optional).

Industry parallels: HashiCorp Vault, AWS Secrets Manager, Tink, and
Salesforce Shield all use explicit-reveal models for sensitive data;
none transparent-decrypt at read time.

### Will my P5-era apps break?

**No.** The pre-PR-3 P5 model had no live creator apps depending on
the transparent-decrypt behaviour — the flip landed before launch.
The migration scanner and the FAQ are here to support apps written
during the P5 transparent-decrypt window (PR 1-4) before P5.5 PR 3
landed.

### How do I declare which actor can unmask?

App-wide policy via `defineMaskPolicy()`. The recommended pattern
is to declare it at the app's entry module so every isolate sees
the same policy on boot:

```ts
import { defineMaskPolicy } from "@zeroship/db";

export default {
  schema: { /* ... */ },
  async startup(env) {
    await defineMaskPolicy(env.db, {
      admin:        ["public", "pii", "spi", "phi", "pci", "internal"],
      support:      ["public", "pii"],
      end_user:     ["public"],
      // `auto` (the system actor) has uniform access UNLESS listed.
      // Listing `auto` with a restricted set narrows the system role.
    });
  },
};
```

The policy is keyed by app id — app A's policy never leaks into
app B's isolate. Roles match the `actor` string passed to
`.unmask()`. Six classifications are available: `public`, `pii`,
`spi`, `phi`, `pci`, `internal` (see the Masking section of
`docs/reference/db.md`).

### What does the audit log capture?

Every `.unmask()` writes one row to `__zeroship_audit_unmask`:

| Column            | Value                                              |
|-------------------|----------------------------------------------------|
| `app_id`          | Tenant id                                          |
| `collection`      | Collection / table name                            |
| `row_pk`          | Numeric primary key of the row                     |
| `column`          | Column name (the parent, not `<col>_masked`)       |
| `classification`  | `"pii" / "spi" / "phi" / "pci" / "public" / "internal"` |
| `actor`           | The caller-supplied actor string                   |
| `reason`          | The caller-supplied reason string                  |
| `outcome`         | `"granted"` or `"denied"`                          |
| `created_at`      | `now()`                                            |

Denied attempts ALSO write a row — that's the security signal.
Granted attempts write a row — that's the compliance signal.

Drift detection (`__zeroship_audit_mask_drift`) is the sibling
table: any time the weekly drift cron finds a sample row whose
stored `<col>_masked` value diverges from the freshly-recomputed
mask, a row lands there with `{ collection, column_name, row_pk,
stored_masked, expected_masked, created_at }`. P6+ surfaces it to
the operator dashboard; for now, query directly:

```sql
SELECT * FROM "<app_id>".__zeroship_audit_mask_drift
ORDER BY created_at DESC LIMIT 100;
```

---

## Related docs

- `docs/reference/db.md` — Masking section (schema declaration,
  the eight mask kinds, the six classifications, `MaskedValue<T>`
  shape).
- `docs/archive/sensitive-field-masking.md` — full design doc
  (the "why" + the "why not the alternatives" + every open
  question); shipped and archived.
- `docs/archive/db-system-design.md` — the parent design doc
  carries an amendment block dated 2026-05-24 with the PR list; shipped and archived.
