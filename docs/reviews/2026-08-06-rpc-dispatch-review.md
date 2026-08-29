# RPC Dispatch Path - Robustness Review (2026-08-06)

Scope: `sdks/bootstrap/src/{dispatcher,fetch-handler,runtime-entry}.ts`,
`sdks/rpc/src/**`, `sdks/vite-plugin/src/{transform,rpc-registry,manifest,build}.ts`,
cross-checked into `crates/zeroship-runtime/src/rpc/**` and `crates/zeroship-gateway/src/router/dispatch.rs`.

Contract checked against `docs/reference/rpc.md` and `docs/reference/zeroship-standard.md`.

NOT yet independently verified - triage before acting.

---

# Review result

Found 18 concrete defects: 9 high, 9 medium. No files were modified.

Important shipping note: [dispatcher.ts:1-24](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:1) claims to be the single production implementation, but the runtime ships a duplicate at [init.rs:503-548](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/core/init.rs:503). Dispatcher findings below exist in both; fixing only the TypeScript copy would not fix production.

## Defects

1. **HIGH — the generated registry publishes plain helper exports.** The transform correctly discovers only wrappers at [transform.ts:1141-1161](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/transform.ts:1141), but the active namespace-walk registry adds every callable export at [rpc-registry.ts:80-87,119-128](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/rpc-registry.ts:80); production invokes it without bindings at [build.ts:444-448](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/build.ts:444). This contradicts [rpc.md:17-19](/home/ruiyang/Projects/appbase/docs/reference/rpc.md:17).
   - Sequence: a `"use server"` module exports wrapped `listTodos` and plain `internalRotateKey`; the manifest contains only `listTodos`, but local/direct-worker dispatch—or a production `*` resource—can invoke `internalRotateKey`.
   - Regression: generated registry keys equal wrapper wire IDs only; dispatching `internalRotateKey` returns `NOT_FOUND` and its side-effect counter remains zero.

2. **MEDIUM — handler lookup accepts inherited registry functions.** [dispatcher.ts:109-114](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:109) indexes a normal object without an own-property check; registries are ordinary `{}` objects at [rpc-registry.ts:119-121](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/rpc-registry.ts:119).
   - Sequence: dispatching `"constructor"` against `{}` invokes inherited `Object`; registering ID `"__proto__"` at [rpc-registry.ts:126-127](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/rpc-registry.ts:126) changes the registry prototype instead of creating an own entry.
   - Regression: `"constructor"`, `"toString"`, and other inherited names return `NOT_FOUND`; `"__proto__"` can only be reached when explicitly registered as an own property.

3. **HIGH — build-resolved capability metadata is not authoritative at runtime.** Wrappers attach `config.kind` at [server.ts:48-75](/home/ruiyang/Projects/appbase/sdks/rpc/src/server.ts:48), but generic `procedure(handler, {id})` is accepted by [server.ts:150-153](/home/ruiyang/Projects/appbase/sdks/rpc/src/server.ts:150). The transform defaults it to mutation at [transform.ts:1317-1336](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/transform.ts:1317) and attaches top-level `.kind` at [transform.ts:1420-1436](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/transform.ts:1420), while the dispatcher reads only `fn.config.kind` at [dispatcher.ts:128,142-145](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:128).
   - Sequence: `procedure(handler,{id:"x"})` is advertised as mutation but can call `fetch`; likewise `q=query(...); q.config={id:"q"}` overwrites the wrapper’s runtime kind while the build still declares query.
   - Regression: generic procedures without `kind` fail compilation/build, and an ID-only post-assignment cannot prevent a query write or mutation fetch from raising `capability_violation`.

4. **HIGH — capability state is thread-global, not invocation-local.** Dispatch enters a kind and awaits the handler at [dispatcher.ts:142-148](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:142). Native state is one thread-local cell at [capability.rs:90-104](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/rpc/capability.rs:90), with snapshot restoration at [capability.rs:217-224](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/rpc/capability.rs:217).
   - Sequence: query A awaits; mutation B enters, resolves A, then awaits. A resumes under `Mutation`, so its write succeeds; A’s out-of-order exit restores `None`, after which B can fetch.
   - Regression: two interleaved deferred handlers must retain their own capability throughout every continuation; both forbidden operations must reject regardless of settlement order.

5. **HIGH — capability exits before an async-generator body executes.** The dispatcher returns an iterator at [dispatcher.ts:147-158](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:147), then immediately exits the capability in [dispatcher.ts:171-173](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:171).
   - Sequence: a query-wrapped async generator performs a write before its first `yield`; generator code starts only when the stream encoder later calls `next()`, after Query has been cleared.
   - Regression: advancing the iterator must retain Query and reject the write; generator cleanup must release the capability only when iteration terminates.

6. **MEDIUM — executable input validation runs outside the capability frame.** Arbitrary `ProcedureSchema.parse` is allowed by [types.ts:9-11](/home/ruiyang/Projects/appbase/sdks/rpc/src/types.ts:9), called at [dispatcher.ts:128-137](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:128), before capability entry at lines 142-145.
   - Sequence: a query schema’s `parse()` starts an `env.db` write, or a mutation schema starts `fetch`, then returns valid input.
   - Regression: capability is already active while `parse()` runs; the forbidden operation rejects and the handler remains uncalled.

7. **HIGH — action kind omission permits a GET CSRF bypass.** [manifest.ts:774-785](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/manifest.ts:774) omits action kind, although the wire enum supports it at [rule.rs:177-185](/home/ruiyang/Projects/appbase/crates/zeroship-bundle/src/rule.rs:177). The gateway checks `Some(Action)` for CSRF but `None` plus GET skips it at [dispatch.rs:1387-1405](/home/ruiyang/Projects/appbase/crates/zeroship-gateway/src/router/dispatch.rs:1387). The contract says action has mutation request shape at [rpc.md:194-199](/home/ruiyang/Projects/appbase/docs/reference/rpc.md:194).
   - Sequence: an action resource has `csrf_origins`; an attacker sends GET without an allowed Origin; absent kind and safe HTTP method skip the guard, and the handler runs.
   - Regression: manifest contains `kind:"action"`; the same GET returns 403 and handler count remains zero.

8. **HIGH — production JSON materialization gives `__proto__` setter semantics.** [superjson.rs:655-663](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/rpc/superjson.rs:655) creates `{}` and calls ordinary `obj.set` for every JSON key.
   - Sequence: send `{"json":{"__proto__":{"isAdmin":true}}}`; V8 invokes the inherited setter, so the handler sees `input.isAdmin === true` and no own `__proto__`.
   - Regression: input retains an own `__proto__` data property, `Object.getPrototypeOf(input) === Object.prototype`, and inherited `isAdmin` is undefined.

9. **MEDIUM — default JSON cannot represent a top-level `json` key.** The client emits bare JSON at [encoding.ts:100-106](/home/ruiyang/Projects/appbase/sdks/rpc/src/encoding.ts:100), while the server treats any object containing `json` as an envelope at [fetch-handler.ts:63-73](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/fetch-handler.ts:63) and [superjson.rs:180-203](/home/ruiyang/Projects/appbase/crates/zeroship-core/src/superjson.rs:180). This violates the normal-JSON claim at [rpc.md:205-214](/home/ruiyang/Projects/appbase/docs/reference/rpc.md:205).
   - Sequence: `{json:"x", other:1}` reaches the handler as `"x"`.
   - Regression: the default-transformer round trip preserves the complete object exactly.

10. **MEDIUM — generated direct-import types disagree with runtime behavior.** `ServerProcedure` accepts one argument and lacks `streamUrl` at [types.ts:62-78](/home/ruiyang/Projects/appbase/sdks/rpc/src/types.ts:62), while generated procedures accept call options and streams expose `streamUrl` at [make-procedure.ts:52-82](/home/ruiyang/Projects/appbase/sdks/rpc/src/make-procedure.ts:52). Both are documented at [rpc.md:101-110,248-250](/home/ruiyang/Projects/appbase/docs/reference/rpc.md:101).
   - Sequence: TypeScript rejects documented `createTodo(input,{idempotencyKey})` with TS2554 and reports `streamProc.streamUrl` missing.
   - Regression: a compile fixture accepts both documented forms while retaining input/output inference.

11. **HIGH — the server stream producer ignores backpressure and cancellation.** [fetch-handler.ts:214-241](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/fetch-handler.ts:214) drains the iterator inside `start()`, never checks `desiredSize`, and defines no `cancel()` hook.
   - Sequence: a fast/unbounded generator fills the response queue for a slow reader; cancelling after one frame does not call `iterator.return()`, leaves its `finally` unrun, and subsequent enqueue targets a closed controller.
   - Regression: production is pull-driven; reader cancellation calls `iterator.return()` exactly once, executes `finally`, and produces no post-cancel enqueue.

12. **MEDIUM — malformed or truncated stream frames fail open as success.** Known-frame JSON parse failures are silently ignored at [transport.ts:477-499](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:477), and EOF without `d:` is declared clean at [transport.ts:577-586](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:577).
   - Sequence: `2:[{"id":1}]\n2:[{"id":` plus EOF yields `{id:1}` followed by normal completion.
   - Regression: malformed known frames and EOF before a valid terminal frame reject with `RpcError` and cancel the reader.

13. **MEDIUM — stream frame buffering is unbounded.** Per-call `buffer` and `pending` have no caps at [transport.ts:367-373,488-498](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:367); chunks are appended until newline at [transport.ts:550-604](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:550), and timeout is optional.
   - Sequence: a 200 response continuously supplies bytes without `\n`; `next()` remains pending while memory grows.
   - Regression: exceeding fixed frame, buffer, or pending-item limits cancels upstream and rejects with a bounded `RpcError`.

14. **MEDIUM — terminal/error frames do not release the response reader.** `e:`/`d:` sets done at [transport.ts:516-541,595-599](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:516), but completion at [transport.ts:632-639](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:632) neither cancels nor releases it. `e:null` dereferences `null` and throws a raw `TypeError`.
   - Sequence: server emits `e:{...}\n` or `d:{}\n` and holds the HTTP body open; iteration terminates while the reader remains locked. `e:null` also bypasses `onError`.
   - Regression: every terminal path cancels/releases exactly once; `e:null` becomes `RpcError(INTERNAL)`, invokes `onError` once, and leaves no timer or reader active.

15. **HIGH — `env.auth` can return another concurrent request’s user.** Users are stored per request at [auth.rs:55-67](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/auth.rs:55), but lookup uses isolate-global `executing_request_id` at [auth.rs:77-105](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/auth.rs:77). The runtime performs an isolate-wide microtask checkpoint under the currently entered request at [runtime.rs:3933-3958](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/core/runtime.rs:3933).
   - Sequence: request B/user B awaits a shared promise; request A/user A resolves it. B’s continuation runs during A’s checkpoint, and `env.auth.getUser()` returns user A.
   - Regression: two same-isolate requests share a gate; B’s result must always be user B and never user A.

16. **MEDIUM — ambient request context is lost when first accessed after an await.** Lazy context exists only during the initial JS call and is cleared at [rpc/dispatch.rs:102-131](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/rpc/dispatch.rs:102); a later first lookup returns undefined at [rpc/dispatch.rs:161-187](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/rpc/dispatch.rs:161). The schema gate at [dispatcher.ts:123-126](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:123) can suspend before the handler even starts.
   - Sequence: `await Promise.resolve(); currentUser()` throws “outside a request handler”; with `__zsSchemaReady`, even a synchronous handler’s first accessor can fail.
   - Regression: first access after an await—and first access after a resolved schema gate—returns the invocation’s original user/request ID.

17. **HIGH — module/bootstrap failures return raw stacks and secrets.** Runtime-entry has concrete boot-time throws at [runtime-entry.ts:94-117](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/runtime-entry.ts:94). Module evaluation replaces the diagnostic with `Error.stack` at [modules.rs:276-293](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/core/modules.rs:276), then returns it directly in a 500 body at [runtime.rs:1823-1837](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/core/runtime.rs:1823).
   - Sequence: top-level initialization throws `Error("dsn=postgres://secret")`; the first RPC caller receives the secret, stack, entrypoint, and module paths in `message`.
   - Regression: initialization failures return the fixed internal envelope; body excludes the injected secret, stack syntax, and absolute paths.

18. **MEDIUM — raw 4xx RPC responses contain server stacks.** Validation creates an `Error` at [dispatcher.ts:92-97,130-137](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:92). The runtime forwards its stack, and only 5xx responses are sanitized at [dispatch.rs:109-145,203-235](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/core/dispatch.rs:109).
   - Sequence: invalid schema input returns 400 JSON containing the dispatcher stack and source/module paths. The typed client ignores the extra field, but a raw caller sees it.
   - Regression: validation responses retain code/message/issues but contain no `stack` field or internal path.

## Checked and found correct

- Transform discovery itself is wrapper-only: [transform.ts:1141-1161,1222-1224](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/transform.ts:1141).
- Wire-ID collisions and missing production IDs fail the build: [manifest.ts:834-891](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/manifest.ts:834).
- Ordinary inline `query`/`mutation`/`action` configs receive a kind, and unary handler exit uses `finally`: [server.ts:48-81](/home/ruiyang/Projects/appbase/sdks/rpc/src/server.ts:48), [dispatcher.ts:142-173](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/dispatcher.ts:142).
- The one-payload-value contract is intentional; conceptual multiple arguments use an object: [transform.ts:1021-1026](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/transform.ts:1021).
- `maxInputBytes` is propagated to `max_input_bytes` and enforced when declared: [manifest.ts:144-149,788-805](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/manifest.ts:144), [dispatch.rs:1408-1415](/home/ruiyang/Projects/appbase/crates/zeroship-gateway/src/router/dispatch.rs:1408). The reference contract specifies no default, so absence of one was not reported.
- Query GET/large-query POST fallback and mutation/action POST match the documented transport: [transport.ts:194-221](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:194).
- Idempotency keys remain stable across retries: [transport.ts:128-142](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:128).
- Stream parser state is allocated inside each `streamCall`, so distinct invocations cannot mix data: [transport.ts:358-373](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:358).
- Frames split across chunks are reassembled on newline boundaries: [transport.ts:588-600](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:588).
- Explicit consumer `return()`/`throw()` cancels upstream: [transport.ts:642-665](/home/ruiyang/Projects/appbase/sdks/rpc/src/transport.ts:642).
- Public SSE mid-stream 5xx errors use the sanitizer: [fetch-handler.ts:230-236,306-345](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/fetch-handler.ts:230).
- Normal unary 5xx failures receive a fixed public body, and the SDK allowlists error fields rather than accepting `stack`: [dispatch.rs:109-144](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/core/dispatch.rs:109), [error.ts:163-180](/home/ruiyang/Projects/appbase/sdks/rpc/src/error.ts:163).
- Per-request auth entries are removed on completion: [auth.rs:70-75](/home/ruiyang/Projects/appbase/crates/zeroship-runtime/src/auth.rs:70).
- Runtime-entry validates corrupt descriptors and removes the privileged DB resolver before handlers run: [runtime-entry.ts:77-122,215-234](/home/ruiyang/Projects/appbase/sdks/bootstrap/src/runtime-entry.ts:77).
- The dormant phase-2 lazy-wrapper metadata loss at [rpc-registry.ts:195-199](/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/rpc-registry.ts:195) was not counted because the current production build does not supply its binding map.

[process exited while detached; exit code unknown]
