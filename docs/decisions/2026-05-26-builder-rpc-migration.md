# Builder RPC Capability Wrappers

## Context

`apps/zeroship-builder` still used the old RPC declaration form:
`export async function fn(...)` plus `fn.config = { id }`. The current
`@zeroship/rpc` transform only registers procedures declared with wrapper
functions from `@zeroship/rpc/server`, so those exports no longer produced
real `/_zs/v1/<id>` endpoints.

The builder also had manual stream typing based on an `App` registry type in
`src/client/api.ts`. The canonical pattern in `examples/db-todos` uses direct
server-function imports for unary calls and per-procedure
`createRpcClient().stream<In, Out>("id")` handles for streams.

## Decision

Declare every public builder RPC procedure with an explicit capability wrapper:

- `action(...)` for handlers that call `fetch()` or delegate to libraries that
  perform outbound HTTP, including control-plane proxies, sandbox-controller
  calls, auth, OpenAI worker passes, and deploy/env/secrets operations.
- `query(...)` for read-only in-process data views that do not call `fetch()`
  and do not write the builder's KV-backed stub stores.
- `mutation(...)` for handlers that write or may seed the KV-backed stub stores.
- `stream(...)` for the chat and wizard SSE endpoints. They return AI SDK
  `Response` streams, but their runtime capability must be the permissive stream
  frame so model and sandbox fetches are allowed.

Internal helpers in underscore-prefixed modules and request/env helpers are not
RPC procedures. They stay as normal exports for server-side imports and carry no
RPC config.

`src/client/api.ts` now creates stream handles directly with
`createRpcClient().stream<In, Out>("chat")` and
`createRpcClient().stream<In, Out>("wizard")`. Unary callers continue to import
server procedures directly and let the Vite transform generate RPC stubs.

## Rationale

`query` and `mutation` run inside database capability frames. They are
transactional and reject outbound `fetch()` so a handler cannot hold a Postgres
transaction open while waiting on a control plane, sandbox controller, or model
provider. Most builder procedures are orchestration/proxy procedures, so
`action` is the correct default for those paths.

The few non-action builder procedures are local dashboard state surfaces:
read-only sample data is `query`, while read-or-seed and write flows are
`mutation`. That keeps the capability declaration aligned to actual side
effects instead of name prefixes like `list*`.

The stream client change removes the retired ad-hoc `App` stream registry while
preserving `streamUrl()` for AI SDK transports.
