# Round 9 - Error handling findings

Total: 8 findings (0 Critical, 3 High, 4 Medium, 1 Low).

Scope walked: async panic sites, public 5xx bodies, DB error classification, ignored async results, wildcard/error catch sites, mutex poisoning, Drop implementations, Builder control/sandbox clients, and sandbox controller/driver surfaces.

## CRITICAL

None found.

## HIGH

### H1. Worker dispatch sends thrown JS messages, stack traces, and details to public app clients

**Files:** `crates/runtime/src/core/dispatch.rs:112`, `crates/runtime/src/core/dispatch.rs:312`, `crates/runtime/src/core/runtime.rs:1744`, `crates/worker/src/handler.rs:240`, `crates/gateway/src/router/dispatch.rs:1135`

**Issue:** An unhandled creator-app exception is serialized into the HTTP response with its raw `message`, `name`, optional `stack`, `code`, `details`, and `retryable`. `v8_exception_to_error_value` reads `.stack` from thrown `Error` objects, `build_error_body` splices it into JSON, `call_fetch_handler` returns that body at the exception's status defaulting to 500, the worker forwards it unchanged, and the gateway returns the worker response unchanged. This leaks stack traces and any internal error text that bubbles through creator code, including native plugin error messages.

**Severity rationale:** Any end user who can trigger a route that throws can receive server-side stack traces and backend/native error strings. That is a direct 500 response disclosure boundary failure, not just operator logging.

**Reproducer:** Deploy an app route that does `throw new Error("dsn=postgres://internal/path")` or lets a native DB error escape. Request the route through the gateway. The response body includes the error message and, for normal JS `Error`s, the stack field.

**Suggested fix:** Introduce a production-safe public error boundary for app HTTP dispatch. For 5xx thrown/rejected errors, log the raw message/stack/details with `request_id` and return a fixed body such as `{"message":"internal error","name":"Error","request_id":...}`. Only expose messages for deliberate public errors, e.g. an explicit `HttpError`/`RpcError` expose flag or non-5xx validation errors. Keep dev-only stack exposure behind an explicit insecure-dev/debug switch.

### H2. Control-plane 5xx/502 responses expose DB, blob-store, VFS, and worker internals

**Files:** `crates/control/src/api.rs:43`, `crates/control/src/registry.rs:33`, `crates/control/src/api.rs:142`, `crates/control/src/api.rs:312`, `crates/bundle/src/unpack.rs:266`, `crates/control/src/api.rs:409`, `crates/control/src/api.rs:461`

**Issue:** Several creator-facing control-plane error paths put raw infrastructure errors directly into response JSON. `RegistryError::from(compio_postgres::Error)` concatenates the Postgres source chain and `error_response` returns `RegistryError::Database(msg)` as a 500 body. Deploy ingest maps blob-store and internal serialization errors into `detail` on 503/500 bodies, with upstream messages like `put_blob_stream({hash}): {e}` and `put_manifest: {e}`. App deletion returns `VfsError::to_string()` in a 500. The app logs endpoint returns every failed worker URL and worker error/body in `details`.

**Severity rationale:** Authenticated creators and Builder RPC callers can see DB source-chain strings, blob-store/VFS paths, worker topology, and upstream worker bodies. This also turns some constraint failures into string-matched 409s with raw duplicate-key text, while non-unique SQLSTATEs fall through to raw 500s.

**Reproducer:** Break a worker log endpoint or make it return a non-2xx body, then call `GET /api/apps/:id/logs`; the response includes `worker_url: HTTP <status> <reason>: <body>`. Similarly, force a non-unique Postgres error in a registry handler and the 500 body contains the concatenated DB source chain.

**Suggested fix:** Split public and private error fields. Return stable public codes/messages for 5xx/502/503 (`database error`, `blob store unavailable`, `worker logs unavailable`) and log raw details with `tracing` plus request/app IDs. Use typed SQLSTATE classification for expected user-visible conflicts/validation instead of substring matching, and keep any duplicate/constraint prose sanitized.

### H3. Sandbox create responses leak raw Docker/Nomad/driver errors despite an existing safe boundary

**Files:** `crates/sandbox/src/handlers.rs:39`, `crates/sandbox/src/handlers.rs:298`, `crates/sandbox/src/handlers.rs:355`, `crates/sandbox/src/handlers.rs:392`, `crates/sandbox/src/handlers.rs:404`, `crates/sandbox/src/backend/nomad_ch.rs:937`, `crates/sandbox/src/backend/nomad_ch.rs:3273`, `crates/sandbox/src/admin_handlers.rs:286`

**Issue:** `POST /sandboxes` routes `CreateOutcome::Failed` through `err(status, code, message)`, and the retry loop builds that `message` from the backend's raw `String` error. Non-retriable failures become `backend.create: {last_err}`; retry exhaustion includes `last error: {last}`. Nomad-CH create errors include probe failures, Nomad allocation terminal descriptions, and driver event messages. The admin surface already documents that raw driver errors can contain host:port, schema, SQL fragments, and binary paths and provides `err_safe`, but create does not use that boundary.

**Severity rationale:** A valid sandbox bearer can receive hypervisor/controller internals, Nomad allocation details, paths, and network topology in normal create failure responses.

**Reproducer:** Configure Nomad-CH unhealthy or force an allocation terminal failure, then call `POST /sandboxes`. The wire response includes `backend.create: ... last error: ...` with the backend's raw diagnostic string.

**Suggested fix:** Make `CreateOutcome::Failed` carry raw diagnostics separately from a fixed public message, or call an `err_safe` equivalent in the create handler. Keep attempt count/status/code public if useful, but log the raw last error via `tracing::warn/error!`.

## MEDIUM

### M1. OIDC callback error pages echo Hydra/JWT token-exchange errors

**Files:** `crates/control/src/api.rs:541`, `crates/control/src/api.rs:572`, `crates/control/src/oidc_rp.rs:173`, `crates/control/src/oidc_rp.rs:179`, `crates/gateway/src/router/dispatch.rs:1249`, `crates/gateway/src/router/dispatch.rs:1298`, `crates/gateway/src/oidc_rp.rs:209`, `crates/gateway/src/oidc_rp.rs:215`

**Issue:** Both the console and app OIDC callbacks log `finish_callback` errors, then render `e.to_string()` into the HTML failure page. The renderer comments say it is generic on purpose and should not leak Hydra strings, but the RP error includes token endpoint status/body (`HTTP {status}: {resp_body}`) and JSON parse failures with `body: {resp_body}`. Verification errors can also expose issuer/audience/JWKS diagnostics depending on the underlying `OidcError`.

**Severity rationale:** Callback failures are externally reachable during login. An attacker can induce malformed callback states or token exchange failures and use the rendered page as an oracle for Hydra/JWT configuration and response details.

**Reproducer:** Trigger a callback where the token exchange returns a non-2xx body or invalid JSON. The sign-in failed page renders the token-exchange string after HTML escaping.

**Suggested fix:** Keep the raw `OidcRpError` in tracing only. Render a fixed public message for token-exchange/verification errors, e.g. `sign-in could not be completed`, while preserving specific safe user-action messages for missing code/state/stash and user-denied consent.

### M2. Builder server RPC forwards raw control and sandbox response bodies to the browser

**Files:** `apps/zeroship-builder/src/server/control-client.ts:42`, `apps/zeroship-builder/src/server/control-client.ts:201`, `apps/zeroship-builder/src/server/apps.ts:51`, `sdks/bootstrap/src/fetch-handler.ts:191`, `sdks/bootstrap/src/fetch-handler.ts:223`, `apps/zeroship-builder/src/server/sandbox.ts:38`, `apps/zeroship-builder/src/server/sandbox.ts:88`, `apps/zeroship-builder/src/server/internal/sandbox-backend.ts:213`, `apps/zeroship-builder/src/server/internal/sandbox-backend.ts:337`

**Issue:** `ControlApiError` uses the upstream response body as `Error.message`, Builder server functions return control-client calls directly, and the bootstrap RPC handler serializes thrown `err.message` into JSON/SSE responses. Sandbox client paths do the same with controller bodies (`sandbox <op> -> <status>: <body>`, `read <path> -> <status>: <body>`, `sandbox create failed (...)`). Tool execution/write failures also include raw controller bodies in user-visible outputs.

**Severity rationale:** This amplifies H2/H3 into the Builder browser/RPC surface. Even after Rust-side fixes, the Builder should not make upstream internals its public error contract by default.

**Reproducer:** Make the control deploy endpoint return a 503 with blob-store `detail`, then call Builder's `deployApp` RPC. The RPC error response body contains the upstream JSON string as `message`.

**Suggested fix:** Store raw upstream bodies on a private field and log them server-side. Public Builder RPC errors should map status/code to sanitized messages (`control request failed`, `sandbox unavailable`, `file read failed`) with a request ID. Preserve typed, expected 4xx validation fields only after an allowlist.

### M3. OAuth client creation silently ignores failed Hydra rollback

**Files:** `crates/control/src/oauth_handlers.rs:121`, `crates/control/src/oauth_handlers.rs:126`, `crates/control/src/oauth_handlers.rs:137`, `crates/control/src/oauth_handlers.rs:198`, `crates/control/src/oauth_handlers.rs:472`

**Issue:** OAuth client creation first creates the Hydra client, then inserts `control.oauth_clients`. If the DB insert fails, the rollback `hydra_delete_client(...).await` is assigned to `let _` and any failure is dropped. A failed rollback leaves a Hydra client that is active but absent from the control DB. The normal delete path first checks the DB row, so that orphan is no longer manageable through this API; a future create sees the DB as absent but Hydra may conflict.

**Severity rationale:** This is an admin-authenticated and multi-failure case, but it can leave externally usable OAuth client state inconsistent with control-plane policy. For public clients (`token_endpoint_auth_method = "none"`), the caller already knows `client_id` and redirect URIs even though the API returned an error.

**Reproducer:** Cause `insert_oauth_client` to fail after Hydra create succeeds, and make Hydra delete fail or time out. The handler returns the DB error response and emits no rollback failure signal.

**Suggested fix:** Treat rollback failure as a separate error: log it at error level with `client_id`, return a clear 500/502 indicating cleanup is required, and preferably persist a cleanup/outbox row before attempting Hydra create or during the same failure path. Consider a reconcile endpoint that can delete Hydra orphans by client ID.

### M4. The runtime HTTP accept loop unwraps transient accept errors

**Files:** `crates/runtime/src/core/serve.rs:1352`, `crates/sandbox/src/preview_ws.rs:95`

**Issue:** The single-tenant/runtime HTTP accept loop does `listener.accept().await.unwrap()`. A transient accept error, listener close, or file-descriptor exhaustion can panic the listener task. Per-connection handling is wrapped in `panic_util::guard`, but the accept loop itself is not. A nearby WS listener handles accept errors by logging and continuing, which is the safer pattern.

**Severity rationale:** This is an async-path panic that can stop serving traffic under resource pressure. It is not a memory safety issue, but it is a preventable availability failure in an executor task.

**Reproducer:** Run the runtime server with a low fd limit and exhaust descriptors until `accept` returns an error. The `.unwrap()` panics instead of logging/backing off.

**Suggested fix:** Replace the unwrap with an error branch that logs, applies a small backoff for repeated transient failures, and only exits on a deliberate shutdown signal. Wrap the accept-loop task itself with `panic_util::guard` as defense in depth.

## LOW

### L1. Several request-path Mutex locks panic on poison instead of recovering or returning an error

**Files:** `crates/control/src/rate_limit.rs:59`, `crates/authz/src/entities.rs:41`, `crates/authz/src/entities.rs:307`, `crates/gateway/src/idempotency.rs:253`, `crates/gateway/src/idempotency.rs:307`, `crates/gateway/src/blob_cache.rs:50`, `crates/core/src/dpop.rs:827`

**Issue:** Request/auth paths use `Mutex::lock().unwrap()` or `.expect("... poisoned")` in rate limiting, authz entity cache, gateway idempotency state, blob caches, and DPoP/logout replay caches. After any panic while holding one of these locks, later ordinary requests panic too.

**Severity rationale:** Poisoning requires a prior panic in the critical section, and most critical sections are small. Still, once poisoned, these become repeatable request-triggered panics in gateway/control/auth paths.

**Reproducer:** Induce a panic while one of these locks is held in a test harness, then issue another request that touches the same cache/store. The subsequent lock acquisition panics.

**Suggested fix:** For caches and replay/idempotency maps, recover with `lock().unwrap_or_else(|p| { tracing::error!(...); p.into_inner() })`, possibly clearing the map if invariants are uncertain. For paths where recovery is unsafe, make the access fallible and return a sanitized 503/500 instead of panicking.

## Areas reviewed and clean

- `crates/plugin-db/src/error.rs` has a substantially better SQLSTATE classifier than the control registry: unique/FK/not-null/check, serialization/deadlock, lock contention, and connection-class errors get stable codes. H1 is the public-response boundary that makes raw plugin messages risky when unhandled.
- Sandbox admin handlers already use `err_safe` for raw DB/hypervisor/snapshot errors; the create path is the notable gap.
- Most `let _ = ...await` instances reviewed are appropriate best-effort cleanup or stream-shutdown cases: temp-file removal, rollback after an already-returning error, WS close/status writes, test teardown, and lock release in cleanup guards. The OAuth Hydra rollback is the material exception.
- Production `Drop` implementations reviewed did not show normal cleanup panics. The notable `expect`/`unwrap` hits around Drop were tests or were using poison recovery in production guards.

## Status

Read-only audit completed. Findings saved in this file; no source code changes were made.

## Status

Fix pass completed on 2026-05-28.

- H1 fixed in `652cd238` (`Sanitize runtime dispatch 5xx errors`) with compile follow-up `93188cc8` (`Fix runtime RPC rejection request IDs`).
- H2 fixed in `b6e9ff85` (`Sanitize control-plane infrastructure errors`).
- H3 fixed in `2388ebd3` (`Sanitize sandbox create failures`).
- M1 fixed in `daec66af` (`Sanitize OIDC callback failure pages`).
- M2 fixed in `b8cbe3a2` (`Sanitize Builder upstream RPC errors`).
- M3 fixed in `19b2533a` (`Report failed OAuth client rollback`).
- M4 fixed in `6258d399` (`Handle runtime accept errors without panic`).
- L1 fixed in `24fb8e2c` (`Recover poisoned request-path mutexes`).

Verification completed where the local environment allowed it:

- Passed: `cargo test -p zeroship-sandbox r9_h3_backend_create_sanitizes_raw_driver_error`
- Passed: `cargo test -p zeroship-sandbox retry_loop_non_retriable_failure_message_is_sanitized`
- Passed: `cargo test -p zeroship-gateway --lib oidc_callback_token_exchange_error_is_generic`
- Passed: `cargo test -p zeroship-core local_jti_cache_recovers_after_lock_poison`
- Passed: `cargo test -p zeroship-authz entity_cache_invalidation_recovers_after_poison`
- Passed: `cargo test -p zeroship-gateway --lib recovers_after`

Blocked verification:

- `zeroship-runtime` tests cannot finish in the default environment because `v8` attempts to download `librusty_v8_release_x86_64-unknown-linux-gnu.a.gz` and this environment has no DNS/network access. With a local `RUSTY_V8_ARCHIVE`, compilation proceeds until generated `sdks/bootstrap/dist/{runtime-entry,dispatcher}.js` files are missing.
- `zeroship-control` tests cannot compile because `crates/control/src/token_handlers.rs` references missing `valid_pat_name`.
- Builder/bootstrap JS tests cannot run because local `node_modules` is absent (`vitest`/`tsx` not found).
- `cargo fmt --check` cannot run because `cargo fmt` is not installed in this environment.
