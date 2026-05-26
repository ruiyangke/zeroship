# API Design Guidelines

Design zeroship APIs so generated code matches the current app model:

- app entry points use the [ZS standard](../reference/zs-standard.md) default export (`schema`, `fetch`, `rpc`, optional `fetchFast`)
- database access hangs off `env.db` after bootstrap installs schema wrappers
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

Use the current default-export shape instead of ad-hoc lifecycle names.

```ts
import { schema, t } from "@zeroship/db";

export default {
  schema: schema({
    users: {
      email: t.string().required().unique(),
      name: t.string().required(),
    },
  }),

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

The typed `env.db` surface is installed by [sdks/bootstrap/src/install-schema.ts](../../sdks/bootstrap/src/install-schema.ts).

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

Zeroship SDK-facing error codes are canonical `SCREAMING_SNAKE`, not prose strings.

```ts
if (error?.code === "UNIQUE_VIOLATION") { /* ... */ }
if (error?.code === "LOCK_NOT_AVAILABLE") { /* ... */ }
```

See the current database-side codes in [sdks/db/src/errors.ts](../../sdks/db/src/errors.ts) and [crates/plugin-db/src/backend/sqlite/error.rs](../../crates/plugin-db/src/backend/sqlite/error.rs).

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

- Is the main path consistent with [docs/reference/zs-standard.md](../reference/zs-standard.md)?
- Is the method name enough for generated code to use it correctly?
- Is there one obvious way to do the operation?
- Are parameters ordered consistently?
- Does the method stay on the established Result-vs-throw rail?
- Are error codes machine-readable and stable?
- Does creator code work without manual setup?
