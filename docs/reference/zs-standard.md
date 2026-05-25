# ZS Standard

The deploy contract is centered on the default export of the app entry module. The runtime bootstrap code is in [sdks/bootstrap/src/runtime-entry.ts](sdks/bootstrap/src/runtime-entry.ts), and the runtime-side loader lives in [crates/runtime/src/core/init.rs](crates/runtime/src/core/init.rs).

## Default export

The current shape is:

```ts
export default {
  schema?,   // optional database schema
  fetch?,    // request handler
  rpc?,      // RPC procedure tree
  fetchFast? // optional fast-path handler
}
```

- `schema` is installed through [sdks/bootstrap/src/install-schema.ts](sdks/bootstrap/src/install-schema.ts).
- `fetch` is the WinterCG-style request handler.
- `rpc` is the procedure tree dispatched by [sdks/bootstrap/src/dispatcher.ts](sdks/bootstrap/src/dispatcher.ts).
- `fetchFast` is still recognized by the runtime in [crates/runtime/src/core/init.rs](crates/runtime/src/core/init.rs) and [crates/runtime/src/core/runtime.rs](crates/runtime/src/core/runtime.rs).

## RPC authoring

`@zeroship/server` provides the current wrapper helpers in [sdks/server/src/index.ts](sdks/server/src/index.ts):

- `procedure`
- `query`
- `mutation`
- `action`
- `stream`
- `subscription`

Named exports are normalized into the runtime RPC tree by [sdks/bootstrap/src/normalize.ts](sdks/bootstrap/src/normalize.ts). The runtime-owned dispatch path is `/_zs/v1/<wireId>`; user code does not route that path manually.

## Current source of truth

`default.schema` is the active schema discovery path. The deprecated manifest-side `exports.schema` field is retained only as a compatibility field in the bundle manifest; it is not the authoring contract for new code. See [crates/bundle/src/manifest.rs](crates/bundle/src/manifest.rs).
