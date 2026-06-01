# Console as a regular app — design (2026-05-30)

**Status:** APPROVED (owner, 2026-05-30). Build order: **MVP control credential (ENV var) → worker kernel
convergence → R5**. Commit-only.

> **2026-05-30 scope revision (owner):** the full runtime-mediated power-token mint (the original "R4")
> is **DEFERRED**. It is too complex to carry right now. The elaborate machinery that was committed —
> the `env.auth.getAccessToken` runtime op, the control-side `POST /internal/power-token` mint +
> `PowerTokenIssuer`, the `AuthzGuard::guard_from_power_token` arm, the `app_session_anchors.auth_time`
> step-up column, and the `@zeroship/auth/server` `getAccessToken`/`fetchAs` surface — has been
> **removed** (forward-removal preserving the kv/storage worker convergence committed after it).
>
> **MVP instead:** the console (R5 slice) reads a **control credential from its server-side app ENV**
> and calls the control plane via the existing **`@zeroship/control`** SDK. **No runtime `env.auth.*`
> op. No control-side power-token mint.** The env-var wiring lands in **R5**. The full power-token design
> is preserved below under **[Future: full-R4 power-token mint](#future-full-r4-power-token-mint)** as
> the eventual target — the knowledge is not lost, only postponed.

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

## The privilege mechanism — MVP (ENV-var control credential)

A plain `@zeroship/db`+`rpc` app has **no** native primitive to mutate the control plane, and giving every
app one (`env.control`) would let any creator app escalate. For the MVP we keep the boundary simple and
ship it on **existing, already-shipped surfaces** — no new runtime op, no new control mint:

1. The console (a first-party app) carries a **control credential in its server-side app ENV** (a
   control-plane master/API key, set as an app env var the worker injects but never exposes to the
   browser — the same `env.*` server-only secret model creator apps already use).
2. Console server code calls the control plane through the existing **`@zeroship/control`** SDK
   (`docs/reference/control.md` · `sdks/control/`), passing that credential as its bearer. No
   `env.auth.getAccessToken`, no `fetchAs`, no `POST /internal/power-token`.
3. The credential lives only in the console's server-side env; it never reaches the console browser
   (the BFF end-user session below is entirely separate from this control credential).

This is deliberately coarser than the per-request, identity-bound, scope-capped token in the deferred
full-R4 design — but it is enough to make the console work first, and it adds **zero new platform
machinery**. The **env-var wiring** (declaring the console's control-credential env var, injecting it
into the deployed console app, and pointing `@zeroship/control` at the internal control URL) lands in
**R5**, alongside the console cutover.

The privilege boundary in this MVP rests on a single property: the control credential is a server-only
app env var that ordinary creator apps are not granted. The richer, per-request identity-bound boundary
is the eventual target — see [Future: full-R4 power-token mint](#future-full-r4-power-token-mint).

## Future: full-R4 power-token mint

> **DEFERRED (full-R4, future).** The text below is the original power-token design, retained as the
> eventual target. It was implemented and then removed in the MVP scope revision (the runtime
> `env.auth.getAccessToken` op, control-side mint + `PowerTokenIssuer`, the `guard_from_power_token`
> AuthzGuard arm, the `app_session_anchors.auth_time` step-up column, and the
> `@zeroship/auth/server` `getAccessToken`/`fetchAs` surface). Revive this when the per-request,
> identity-bound, scope-capped credential is worth its complexity.

Elevation lives in an unforgeable, server-side, identity-bound credential — the Supabase `service_role` /
Cloudflare capability-binding pattern. Concretely:

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
`__Host-zeroship_app_session` (+ `__Host-zeroship_app_anchor`) cookies and a `{ user }` projection. Worker code sees
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
  `control-client.ts` bearer/refresh loop. Replace control-plane calls with the **`@zeroship/control`**
  SDK authenticated by the console's server-side **control-credential env var** (MVP). _(Eventual target:
  `auth.getAccessToken({audience, scopes:[least-privilege per-op]})` + `fetchAs` once full-R4 is revived.)_
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
- **Internal reachability:** in the MVP the console reaches control via `@zeroship/control` over an
  **allowlisted internal control hostname** with its server-side control-credential bearer — no
  app-level SSRF carve-out is created. The sandbox controller + any other internal services the console
  calls are likewise reached via **allowlisted public hostnames**. _(The deferred full-R4 mint instead
  rode the Rust-side worker→control internal channel, bypassing the JS SSRF guard — see Future.)_

## Phased path

1. **MVP control credential (was "R4") — DEFERRED full power-token mint.** The full runtime-mediated
   power-token mint is **deferred** (see [Future: full-R4 power-token mint](#future-full-r4-power-token-mint)).
   The MVP instead: the console reads a **control credential from its server-side app ENV** and calls
   control via **`@zeroship/control`** — no runtime `env.auth.*` op, no control-side mint. The **env-var
   wiring** (declare + inject the console's control-credential env var; point `@zeroship/control` at the
   internal control URL) lands in **R5**, with the console cutover.
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

- **Privilege-isolation correctness is load-bearing** (shared pool). In the MVP the boundary is the
  server-only control-credential env var that ordinary creator apps are not granted; the E2E
  privilege-boundary test is the guard. _(The deferred full-R4 design's three invariant pillars + its
  regression test are the eventual, stronger guard — see Future.)_
- **The kv/storage worker gap is a genuine prod gap** — sequence kernel convergence BEFORE the console
  seed, or the console's KV read model silently fails in prod despite working in dev.
- **Long-running AI gen depends on the SSE stream staying I/O-bound** — verify the deepagents/chat loop does
  no synchronous in-isolate compute beyond stream framing.
- **The bootstrap seed is a privileged non-interactive path** — keep it strictly in-process at control
  boot, gated by the flag, never an HTTP route, or it becomes an authz bypass.
- **The oidc_rp.rs / require_console_session rip is a coordinated multi-file removal** — do it in the same
  patch as the cutover to avoid a split-brain auth window.
