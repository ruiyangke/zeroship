---
name: zeroship-rpc
description: Use when adding or changing a server function in a zeroship app (query, mutation, action, stream), when choosing a capability wrapper, or when a procedure works locally but returns 401 once deployed. Covers the wrappers and their limits, stable wire ids, input validation, and the fail-closed auth policy.
---

# Server functions (RPC)

Every exported wrapped function in a `"use server"` module becomes an RPC
procedure. Plain unwrapped exports stay server-private.

```ts
"use server";
import { query, mutation } from "@zeroship/rpc/server";
import { env } from "zeroship";

export const listNotes = query(async () => {
  const { data, error } = await env.db.notes.find().sort({ id: -1 });
  if (error) throw error;
  return data ?? [];
}, { id: "notes.list" });
```

## Pick the wrapper by capability

Wrappers come from `@zeroship/rpc/server`. The wrapper is a capability tag the
runtime enforces, not a hint.

| Wrapper | Read `env.db` | Write `env.db` | `fetch()` |
| --- | --- | --- | --- |
| `query` | yes | **refused** | **refused** |
| `mutation` | yes | yes | **refused** |
| `action` | yes | yes | yes |
| `stream` | yes | yes | yes |

Refusals are enforced at request time and surface as a capability violation:

- A `query` is strictly read-only. It cannot write, and it cannot call `fetch`.
- A `mutation` can write but cannot call `fetch`.
- `action`, `stream` and `subscription` may do both.

So reach for `action` when a handler needs an external HTTP call. Composing its
database work through `runQuery` / `runMutation` from `@zeroship/server` keeps
each step inside the right capability, which is good practice, though a direct
write from an `action` is not refused.
- `subscription` and `streamResponse` also exist, for long-lived feeds and raw
  response streams.

**No wrapper opens a transaction.** Top-level `env.db.<collection>.*` calls
autocommit one operation at a time. When several writes must commit or roll
back together, call `env.db.transaction()` yourself. See `zeroship-data`.

## Every procedure needs an explicit id

The id is the wire identity: the procedure is served at
`/__zeroship/v1/<id>`. Use dotted, stable ids such as `notes.list`.

A production build refuses to package a procedure whose id was defaulted from
the export name, and names each offender. Fix it by adding the id, never by
renaming the export.

## Auth: authenticated by default, and dev will not tell you

Behind the gateway, an RPC procedure whose policy chain declares no `auth`
resolves to `auth: "user"`. That default is deliberate: a forgotten policy
must be a loud 401, never a silent public endpoint.

**`pnpm dev` runs no gateway and does not enforce this.** A procedure with no
policy works perfectly on your machine and returns 401 for every caller once
deployed. The build prints a warning naming each such procedure. Do not ignore
it.

To make a procedure public, declare both keys in the app's resource policy. The
wrapper config accepts `auth`, but `publiclyAccessible` is a resource-tree field
only, so the confirmation lives in `src/server/config.ts`:

```ts
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:notes": { auth: "anonymous", publiclyAccessible: true },
  },
});
```

`auth: "anonymous"` without `publiclyAccessible: true` fails a production
build: the flag is the deliberate confirmation that the endpoint is meant to be
world-callable.

For user-scoped data, leave the default and read identity inside the handler
with `env.auth.getUser()` or `env.auth.requireUser()`.

### Where to declare the policy

Per-procedure `auth` belongs at the call site, in the wrapper's config:

```ts
export const listNotes = query(handler, { id: "notes.list", auth: "user" });
```

It cannot drift from the procedure it governs, and it has no inheritance to
reason about. The public confirmation is the exception: `publiclyAccessible` is
a resource-tree field, so it goes in `defineApp`.

Use `defineApp({ resources })` in `src/server/config.ts` for what a call site
cannot express:

```ts
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:notes": { auth: "user" },              // a whole dotted family
    "/health": { auth: "anonymous", publiclyAccessible: true },
  },
});
```

That tree also covers the `*` root, non-RPC URL paths, static/redirect/rewrite
actions, and app-wide RPC defaults. Both inputs are merged and the resource
tree wins, so a resource-tree entry overrides the procedure's own config for the
fields it names.

Be careful with a `*` root: policy is inherited, so a root that declares
`auth: "anonymous"` makes every procedure in the app anonymous, including ones
you never thought about. Prefer narrow keys.

## Validating input

Set `input` / `output` on the config to a Zod schema. Zod is an optional peer
dependency, re-exported for convenience:

```ts
import { z } from "@zeroship/server";

export const addNote = mutation(handler, {
  id: "notes.add",
  input: z.object({ title: z.string().min(1).max(200) }),
});
```
