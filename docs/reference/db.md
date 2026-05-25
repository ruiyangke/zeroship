# `@zeroship/db`

`@zeroship/db` is the typed database SDK. Creators declare schema in `export default { schema }`; the runtime and bootstrap layer install typed collections onto `env.db` before requests are handled. The current implementation lives in [sdks/db/src/index.ts](sdks/db/src/index.ts), [sdks/bootstrap/src/install-schema.ts](sdks/bootstrap/src/install-schema.ts), and [crates/plugin-db/src/lib.rs](crates/plugin-db/src/lib.rs).

## Authoring surface

```ts
import { schema, t } from "@zeroship/db";

export default {
  schema: schema({
    users: {
      name: t.string(),
      createdAt: t.timestamp(),
      tags: t.array(t.string()),
    },
  }),
};
```

Current schema builders are defined in [sdks/db/src/types.ts](sdks/db/src/types.ts). Verified builders include `t.string()`, `t.number()`, `t.boolean()`, `t.timestamp()`, `t.json()`, `t.array(...)`, `t.ref(...)`, `t.object(...)`, `t.vector(...)`, `t.geoPoint()`, `t.calendarDate()`, `t.bytes()`, `t.encrypted(...)`, `t.literal(...)`, and `t.union(...)`.

Collection-level options are also defined in [sdks/db/src/types.ts](sdks/db/src/types.ts):

- `.index(name, fields)`
- `.uniqueIndex(name, fields)`
- `.softDelete()`
- `.withVersioning()`
- `.strictness(level)`

`softDelete` and `versioning` are opt-in. They are not enabled by default.

## Runtime surface

The low-level native handle is registered by [crates/plugin-db/src/lib.rs](crates/plugin-db/src/lib.rs). Creator code normally uses the typed wrappers that bootstrap installs onto `env.db`, not the raw native class.

Verified collection methods are implemented in [sdks/db/src/collection.ts](sdks/db/src/collection.ts) and typed in [sdks/types/db.d.ts](sdks/types/db.d.ts):

- `insert`, `insertMany`
- `get`, `exists`
- `find`
- `upsert`
- `update`, `updateMany`
- `delete`, `deleteMany`
- `purge`, `purgeMany`
- `restore`, `restoreMany`
- `count`, `distinct`, `aggregate`
- `search`, `near`
- `bulkUnmask`

The low-level fallback `env.db.collection(name)` is still available. Transactions are exposed as `env.db.transaction(fn, options?)`.

Live queries and subscriptions are provided by the SDK, not a top-level `env.db.subscribe(...)` helper:

- `db.live(...)` in [sdks/db/src/live.ts](sdks/db/src/live.ts)
- `subscribe(collection)` in [sdks/db/src/subscribe.ts](sdks/db/src/subscribe.ts)
- `collection(name).openSubscription()` at the native boundary

## Current boundary

Platform-only operations such as model registration, masking, migrations, and replication no longer belong to the creator-facing `env.db` surface. Bootstrap resolves those through the private platform handle in [sdks/bootstrap/src/install-schema.ts](sdks/bootstrap/src/install-schema.ts), and direct string access to `env.db.__platform` is intentionally blocked in the native layer.
