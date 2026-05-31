# Console as a regular app — design (2026-05-30)

**Status:** APPROVED (owner, 2026-05-30). Build order: **R4 → worker kernel convergence → R5**. Commit-only.

The console / app-builder becomes a **regular zeroship app on the standard dev+prod runtime** —
authored with `@zeroship/{db,rpc,kv,ui}`, built by `@zeroship/vite-plugin` into a `.zship`, deployed
to blob storage, route-synced, dispatched by the gateway, run in the V8 worker — with its
creator/control-plane authority granted **only** through a narrowly-gated, BFF-minted, identity-bound
capability that no ordinary creator app can obtain. Shorthand: *"regular app on the standard runtime,
first-party privilege tier."* Not *"regular app that magically inherits control-plane power."*

This formalizes the grounded analysis (workflow `wci5rpjec`, 6 probes + synthesis, high confidence) and
maps onto the existing reshape slices: **R5 = console collapse**, with **R4 = grant-gated platform
capabilities** promoted to its prerequisite.

## Why (and the pleasant surprise)

The console is *already* authored to the ZS-standard contract — `apps/zeroship-builder/src/server.ts`
is discovered by the vite-plugin (`export { builderFetch as fetch }` + `"use server"` modules), already
uses `@zeroship/kv` + `@zeroship/ui`, and `package.json` already has `"deploy": "vite build && zeroship
deploy"`. Its heavy subsystems (sandbox, AI codegen) are already network-bound RPC procedures proxying
over `fetch` to the external `zeroship-sandbox` controller + LLM APIs (chat is SSE). Nothing spawns
processes or touches the filesystem inside the worker. **So this is a cutover, not a port** — the
authoring/dev side is essentially done; the work is the privilege layer, the kernel skew, the bootstrap
seed, and ripping out the bespoke auth.

The dogfood payoff: every gap the console hits (worker capability envelope, multi-module deploy, dev/prod
skew) is a gap a creator would hit — found first-party, on the platform's own SDKs.

## The privilege mechanism (R4)

A plain `@zeroship/db`+`rpc` app has **no** native primitive to mutate the control plane, and giving every
app one (`env.control`) would let any creator app escalate. So elevation lives in an unforgeable,
server-side, identity-bound credential — the Supabase `service_role` / Cloudflare capability-binding
pattern. Concretely:

1. Console server code calls `@zeroship/auth/server` `getAccessToken({ audience: CONTROL_PLANE_AUDIENCE,
   scopes: [...] })` (a JS `fetch()` wrapper, NOT a native `env.auth` op — the plugin is synchronous; this
   is async).
2. That issues a worker→control `POST /internal/power-token`, gated by `control_key` on the **existing**
   `internal.rs` router. `control_key` stays Rust-side, **never JS-visible**.
3. Control re-derives the user from the echoed, **gateway-signed `ZeroShip-User` header** (HMAC over
   `worker_key` — only the gateway can mint it, the worker echoes it, control verifies with the same
   verifier the worker already uses at `worker/src/handler.rs:65`). It does **not** trust the worker's word.
4. Control enforces the grant ceiling (`anchor.granted_scopes`) per-op via `AuthzGuard.require(Action,
   Resource)`. The minted `aud=control` bearer is used server-side and **never reaches the console browser**.
5. **Step-up** (fresh `auth_time` + elevated scope) is required for deploy / secret-rotation / Stripe /
   delete.

Two elevations stay distinct: *"operator is logged in"* (gateway BFF session) vs *"this app may mutate the
control plane"* (R4 grant). The control-plane bearer is the latter and is short-lived + scope-capped.

**Security invariant (the load-bearing property):** an ordinary creator app **cannot** mint an
`aud=control` token. The console's authority is identity-bound and grant-capped per request, so even
co-tenanting its isolate with untrusted creator isolates leaks no standing privilege. This rests on three
things, all of which must hold: (a) `control_key` is never JS-visible; (b) control re-derives identity from
the signed `ZeroShip-User` header, not the worker's claim; (c) the grant ceiling caps scopes.

## End-user (creator) auth under BFF

The console becomes a **first-party pseudo-app**: a platform-seeded `control.apps` row for
`console.zeroship.{ai|localhost}` with its own `oauth_client_id` (audience incl. `CONTROL_PLANE_AUDIENCE`)
and an **explicit `sector_identifier`** (the seed passes the host explicitly rather than deriving
`{name}.{base}`, since the console host is reserved / 2-label). Creators log in via the standard
`@zeroship/auth` popup/consent flow on the console host and hold **no power token** — only the HttpOnly
`__Host-zs_app_session` (+ `__Host-zs_app_anchor`) cookies and a `{ user }` projection. Worker code sees
only `ZeroShip-User`.

## Bootstrap (install-time seed)

The console deploys apps, so the console can't deploy itself, and `AuthzGuard` has no master-key deploy
bypass. Break the cycle with an install-time **`--bootstrap-console`** control path (mirroring the existing
`bootstrap_builder.rs` precedent), run in-process/trusted at control boot — **never an HTTP route**:

- INSERT the `console.apps` row (platform-owned/privileged flag).
- INSERT the `app_oauth_clients` row (public PKCE client, explicit apex host + `sector_identifier`,
  audience incl. `CONTROL_PLANE_AUDIENCE`).
- Ingest the prebuilt console `.zship` blob + set `deploy_hash`, bypassing the authz-gated HTTP deploy
  handler (which has no non-interactive principal).
- Compose ordering: build the console `.zship` in the frontend image; run the seed after
  migrate+control+worker+gateway are healthy.

## Cutover (deletions — same patch, no shim, pre-launch)

- DELETE `apps/zeroship-builder/src/server/{oauth.ts,session.ts,oauth-store.ts}` + the bespoke
  `control-client.ts` bearer/refresh loop. Replace control-plane calls with `auth.getAccessToken({audience,
  scopes:[least-privilege per-op]})` + `fetchAs`.
- RIP OUT `crates/control/src/oidc_rp.rs` + `require_console_session` + the confidential console client in
  `ops/auth-clients-dev.toml`. **Control becomes a pure API resource server.**
- Repoint `ops/Caddyfile` console block (and prod DNS) from `control:9090` to `gateway:8000`; the gateway
  serves `console.*` via `lookup_by_name` on the full host once the route entry exists (no auth-style
  carve-out).
- Fix the stale `env_handlers.rs:3` comment ("Mutations require master key" → the AuthzGuard model).

## Dev + prod story

- **Dev:** unchanged — the `@zeroship/vite-plugin` dev runtime (dev-bootstrap spawns the `zeroship`
  runtime), so the console dev loop dogfoods the same dispatcher/bootstrap as prod.
- **Prod:** the built `.zship` is deployed and served gateway→worker. Console runs on the
  unlimited/enterprise plan (no CPU/wall cap via `registry.rs runtime_limits_for_plan`) — its long-running
  work is the SSE chat stream (I/O-bound forwarding of upstream LLM bytes) + sandbox calls over `fetch`,
  not in-isolate CPU.

## Decisions locked (owner, 2026-05-30)

- **Realtime: SSE-over-fetch is sufficient.** Chat already streams over SSE-fetch through the gateway. The
  gateway's multi-node WS-subscription proxy (currently a 501 stub) is NOT a console blocker.
- **Worker isolation: shared pool, no dedicated trusted tier.** Safety rests entirely on the
  no-ambient-authority design above. The platform-privileged flag exists ONLY to permit declaring reserved
  scopes (a creator app declaring `secrets:write` is rejected) — not for tier pinning.
- **Console plan: unlimited/enterprise** (no CPU/wall cap).
- **Internal reachability:** the control-plane mint goes through the existing worker→control internal
  channel (Rust-side `control_url`/`control_key`), bypassing the JS SSRF guard. The sandbox controller +
  any other internal services the console calls are reached via **allowlisted public hostnames** — no
  app-level SSRF carve-out is created.

## Phased path

1. **R4 — grant-gated platform capabilities (prerequisite).** Control-side `POST /internal/power-token`
   mint on `internal.rs` (`control_key`-gated) with user re-derivation from the signed `ZeroShip-User`
   header + grant-ceiling enforcement via `AuthzGuard.require`. Surface `control_url`/`control_key` to the
   runtime SDK layer (Rust-side only). Ship `@zeroship/auth/server` `getAccessToken`/`fetchAs` as a JS
   `fetch()` wrapper. Add step-up (fresh `auth_time` + elevated scope) for deploy/secret-rotation/Stripe/
   delete. Regression test: an ordinary creator app cannot mint an `aud=control` token.
2. **Worker kernel convergence (blocking, independent value).** Register `KvPlugin` + `StoragePlugin` in
   `crates/worker/src/cache.rs create_plugins()` so the multi-node worker exposes the full
   `env.{db,kv,storage,auth}` kernel that single-tenant `zeroship serve` already does. Removes a real
   dev/prod capability skew. Regression test: all four namespaces resolve in a worker-served app.
3. ~~Gateway realtime~~ — skipped per the SSE decision (audit only if a hard WS dependency surfaces).
4. **R5a — install-time console seed** (`--bootstrap-console`, see Bootstrap).
5. **R5b — repoint routing** (Caddy/DNS console block → gateway).
6. **R5c — collapse the bespoke console auth** (see Cutover; same patch).
7. **R5d — runtime envelope** (enterprise plan; platform-privileged flag for reserved scopes; shared pool).
8. **E2E (faithful path).** Exercise the REAL deployed console `.zship` through gateway→worker: login via
   the standard popup flow, a least-privilege control read (`apps:read`), a step-up-gated deploy, the SSE
   chat stream, a sandbox proxy round-trip — and assert (a) the control-plane bearer never reaches the
   browser and (b) an ordinary creator app cannot mint an `aud=control` token.

## Risks & mitigations

- **Privilege-isolation correctness is load-bearing** (shared pool). The three invariant pillars above must
  all hold; the R4 regression test + the E2E privilege-boundary test are the guards.
- **The kv/storage worker gap is a genuine prod gap** — sequence kernel convergence BEFORE the console
  seed, or the console's KV read model silently fails in prod despite working in dev.
- **Long-running AI gen depends on the SSE stream staying I/O-bound** — verify the deepagents/chat loop does
  no synchronous in-isolate compute beyond stream framing.
- **The bootstrap seed is a privileged non-interactive path** — keep it strictly in-process at control
  boot, gated by the flag, never an HTTP route, or it becomes an authz bypass.
- **The oidc_rp.rs / require_console_session rip is a coordinated multi-file removal** — do it in the same
  patch as the cutover to avoid a split-brain auth window.
