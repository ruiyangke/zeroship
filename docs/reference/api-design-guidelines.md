# API Design Guidelines

Design zeroship APIs so generated code matches the current app model:

- app entry points use the [zeroship standard](../reference/zeroship-standard.md) default export (`fetch`, `rpc`)
- database access hangs off typed `env.db`, generated from committed migrations
- auth is an explicit SDK call (`auth.getUser()` / `auth.requireUser()`)

The goal is still the same: one obvious way to do the common thing, with names that read naturally in generated code.

## Principles

### 1. Prefer one obvious path

If two shapes do the same job, keep one.

```ts
// Bad
db.query("SELECT * FROM users WHERE id = $1", [id]);
db.collection("users").find({ id });

// Good
const { data: user } = await env.db.users.get(id);
```

### 2. Match the zeroship entry contract

Use the current default-export shape instead of ad-hoc lifecycle names. Schema
changes belong in migrations, not in the app entry default export.

```ts
export default {
  async fetch(request, env, ctx) {
    return Response.json({ ok: true });
  },
};
```

### 3. Keep auth explicit

Do not rely on request mutation or hidden globals.

```ts
// Bad
const user = request.user;

// Good
import { auth } from "@zeroship/auth";

const user = auth.requireUser();
```

The current auth helper lives in [sdks/auth/src/index.ts](../../sdks/auth/src/index.ts).

### 4. Zero setup for creator code

Creator code should import the SDK and call it. No manual client construction, no connection setup, no middleware registration.

```ts
import { env } from "zeroship";

const { data, error } = await env.db.users.insert({
  email: "alice@example.com",
  name: "Alice",
});
```

The typed `env.db` surface is generated from the migration fold into
`generated/zeroship/env.db.ts` and installed at runtime from
`schema.runtime.json`.

### 5. Use names that read like English

Collection methods should read naturally:

- `get(idOrFilter)`
- `find(filter)`
- `insert(row)`
- `upsert(row, { conflictFields })`
- `update(idOrFilter, patch)`
- `updateMany(filter, patch)`
- `delete(idOrFilter)`
- `deleteMany(filter)`
- `count(filter?)`
- `exists(filter?)`

The public collection surface is defined in [sdks/db/src/collection.ts](../../sdks/db/src/collection.ts), with the `Db` / `Collections` / `TxCollection` types in [sdks/db/src/db-types.ts](../../sdks/db/src/db-types.ts).

### 6. Keep parameter order stable

Use `(what to match, what to do)` for mutations.

```ts
await env.db.users.update("usr_123", { name: "Alice" });
await env.db.users.updateMany({ role: "guest" }, { role: "member" });
```

Avoid signatures that swap row/filter order between methods.

### 7. Make return rails predictable

Outside transactions, collection operations stay on the `Result` rail. Inside `env.db.transaction(...)`, `tx.*` methods return data directly and throw on error.

```ts
const { data, error } = await env.db.users.get("usr_123");

const txResult = await env.db.transaction(async (tx) => {
  const user = await tx.users.insert({ email: "a@b.com", name: "Alice" });
  return user.id;
});
```

### 8. Prefer structured input over string DSLs

```ts
// Bad
schema({
  users: {
    email: "string! unique index",
  },
});

// Good
schema({
  users: {
    email: t.string().required().unique(),
  },
});
```

### 9. Use stable machine-readable error codes

Zeroship surfaces error codes from two namespaces:

- **SDK error-class codes** — `SCREAMING_SNAKE` literals stamped by the typed
  error classes in [`sdks/db/src/errors.ts`](../../sdks/db/src/errors.ts):
  `VALIDATION` (`ValidationError`), `OPTIMISTIC_CONCURRENCY`
  (`OptimisticLockError`), `NOT_FOUND` (`NotFoundError`), `NOT_UNIQUE`
  (`NotUniqueError`).
- **Native DB wire codes** — `lower_snake_case` strings the runtime stamps from
  the `DbError` variant in [`crates/plugin-db/src/error.rs`](../../crates/plugin-db/src/error.rs):
  `unique_violation`, `fk_violation`, `not_null_violation`, `check_violation`,
  `serialization_failure`, `lock_not_available`, `transient`, …

The SDK's `mapNativeError` rail runs caught errors through `canonicalErrorCode`,
which upcases the wire code, so app code branches on the `SCREAMING_SNAKE` form
either way:

```ts
if (error?.code === "UNIQUE_VIOLATION") { /* unique_violation, canonicalized */ }
if (error?.code === "OPTIMISTIC_CONCURRENCY") { /* OptimisticLockError class */ }
```

Whichever namespace an error originates in, prefer matching `error.code` over
substring-matching the message.

### 10. Let query chains read left-to-right

```ts
const { data } = await env.db.recipes
  .find({ category: "dessert" })
  .sort({ rating: -1 })
  .limit(20);
```

Each step should narrow or shape the result; no hidden execution until the query is awaited.

## Current Package Names

Use the packages that exist today:

- `@zeroship/db`
- `@zeroship/auth`
- `@zeroship/storage`
- `@zeroship/kv`
- `@zeroship/payments`
- `@zeroship/server`
- `@zeroship/rpc`

Keep names short, lowercase, and concrete.

## Anti-Patterns

### Don't overload one method with unrelated meanings

```ts
// Bad
find();
find(id);
find({ role: "admin" });

// Good
get(id);
get({ email: "a@b.com" });
find({ role: "admin" });
```

### Don't require setup before first use

```ts
// Bad
const db = new Database();
await db.connect();

// Good
const { data } = await env.db.users.count();
```

### Don't use positional booleans

```ts
// Bad
users.find({ role: "admin" }, true, false, 20);

// Good
users.find({ role: "admin" }).sort({ name: 1 }).limit(20);
```

### Don't mutate caller input

Filters, patches, and row objects should be treated as inputs, not scratch space.

## Checklist

- Is the main path consistent with [docs/reference/zeroship-standard.md](../reference/zeroship-standard.md)?
- Is the method name enough for generated code to use it correctly?
- Is there one obvious way to do the operation?
- Are parameters ordered consistently?
- Does the method stay on the established Result-vs-throw rail?
- Are error codes machine-readable and stable?
- Does creator code work without manual setup?
