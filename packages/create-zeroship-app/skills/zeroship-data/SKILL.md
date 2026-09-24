---
name: zeroship-data
description: Use when changing a zeroship app's database schema or writing env.db queries - authoring migrations as the schema source of truth, the generated typing, system columns, relations, the Result shape, transactions, and delete vs purge. Load before editing anything under migrations/ or calling env.db.
---

# Data: migrations and env.db

`env.db` is typed structured CRUD. There is no raw SQL surface.

## Migrations are the schema source of truth

You do not export a schema object from app code. You author a migration under
`migrations/`, and the toolchain folds every migration into the generated
artifacts that type `env.db` and drive the runtime.

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "add_notes",
  schema() {
    table("users").create({
      columns: {
        email: t.text().required().unique(),
        name: t.text().required(),
      },
    });

    table("notes").create({
      columns: {
        userId: t.text().required().references("users", "id", { relation: "author" }),
        title: t.text().required(),
        body: t.text(),
        pinned: t.boolean().required().default(false),
      },
      indexes: [{ name: "notes_user_idx", on: ["userId"] }],
    });
  },
};
```

Column builders on `t` include `text`, `string`, `boolean`, `int`, `bigInt`,
`smallInt`, `real`, `numeric`, `timestamp`, `date`, `uuid`, `json`, `bytes`,
`textArray`, `vector`, `geoPoint`. Modifiers chain: `.required()`, `.default()`,
`.unique()`, `.primaryKey()`, `.references()`, `.mask()`, `.generated()`.

To change the schema, add a migration. Never hand-edit anything under
`generated/zeroship/`; the build regenerates it and a production build fails if
those artifacts drift from the migration source.

## System columns are added for you

Do not declare these. Every table gets them:

`id` (a typed id such as `note_...`), `created_at`, `updated_at`, `created_by`,
`updated_by`, `version`, and `deleted_at`.

## Reading and writing

Outside a transaction every call returns a `Result`, so unwrap it. This is the
single most common mistake:

```ts
const { data, error } = await env.db.notes.find({ pinned: true }).sort({ id: -1 });
if (error) throw error;
return data ?? [];
```

Sort by `id` rather than `created_at` for stable feed ordering: ids are
monotonic, and timestamps tie.

Load a relation declared with `.references(..., { relation })` by name:

```ts
const { data } = await env.db.notes.find({ userId }, { with: { author: true } });
```

## Transactions

No RPC wrapper opens a transaction. Top-level calls autocommit one operation at
a time. When several writes must commit or roll back together, ask for it:

```ts
const result = await env.db.transaction(async (tx) => {
  const note = await tx.notes.insert({ userId, title });
  await tx.users.update(userId, { noteCount: n + 1 });
  return note;
});
if (result.error) throw result.error;
```

- Resolving commits; throwing rolls back. There is no `tx.commit()`.
- **Inside the callback, collections throw instead of returning a `Result`.**
  Do not destructure `{ data, error }` off `tx.*` calls.
- A nested `transaction()` becomes a savepoint, so an inner failure rolls back
  only the inner work. Depth is bounded by `MAX_SAVEPOINT_DEPTH`.

## Delete is physical by default

`delete` removes the row. It only marks the row instead when a column declares
the soft-delete role, in which case reads filter it out automatically. `purge`
always removes the row regardless.

## Schema changes need a migrate as well as a deploy

The built artifact carries the generated typing, not the migrations, and
deploying does not apply them. Run `zeroship migrate` every time the schema
changes. It targets the DATABASE, not an app, so it needs no prior deploy and
the two commands run in either order. See `zeroship-deploy`.
