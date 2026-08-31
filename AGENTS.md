# zeroship

A platform where anyone can create, launch, and run software without writing code. Creators describe what they want in natural language. AI builds it. The platform handles the infrastructure: hosting, database, auth, payments, and scaling. It meters the infrastructure each app consumes, and integrates Stripe (including Stripe Connect) so apps can accept payments from their end users.

**Primary creator flow (2026-06-29 direction): build locally → deploy.** Creators (or an AI coding agent — Claude Code / Codex — on the creator's machine) build a zeroship app locally and `zeroship deploy` the built `.zship` to the platform. We don't rebuild a hosted in-browser AI builder (the agents do that better); the platform's value is the *infrastructure* (runtime, `env.*` primitives, deploy contract, gateway, billing). The hosted-build environment / **sandbox is deferred** (extracted to the standalone `zeroship-sandbox` project). Golden path + scaffold: `docs/build-and-deploy-golden-path.md` · `examples/starter/` (+ its `CLAUDE.md`) · `tests/golden_path.sh`.

This file is the AI-agent landing page. Read the **task router** below first.

---

## Development status — pre-launch, no back-compat

**Zeroship has never been published.** No production users, no production tenants, no creator apps in the wild. Every API, every wire format, every schema is fair game to break.

This is a deliberate stance — not a limitation. Pre-launch is the moment to get shapes right. Building back-compat infrastructure for users that don't exist is wasted work.

**What this does NOT license.** This section is about *back-compat obligations*, nothing else. "We are pre-launch" is never a reason to defer security, correctness, or a design done properly. We are closing the gaps *to* production, not deferring them past it. For anything a launch would freeze - credential and trust models, wire formats, schema shapes - the argument above runs the *other* way: changing them afterwards means migrating live tenants and coordinating rollout across every deployment, so now is when it is cheapest, not when it is most deferrable. Build the end state; do not build two intermediate versions and throw both away. Sequencing work by real dependencies is right; sequencing it by "later, because pre-launch" is not.

**Implications for any change you propose:**

- **No `@deprecated` aliases.** Rename the symbol. Delete the old name. One PR.
- **No migration shims, no detect-and-warn paths, no "legacy mode" fallbacks.** If the new shape is right, the old one disappears in the same PR.
- **No back-compat shims for wire format / SDK contract / V8 RPC.** Break the shape and update every caller in the same change.
- **No "scan the creator codebase" CLI tooling.** There are no creator codebases.
- **No "ALTER existing tables for new system fields" backfills.** No creator app has tables in production. The detect-and-warn arms that exist today (plugin-db PR 5 Path C, the unreached migration scanners) are dead code waiting for the post-launch simplification pass.
- **Wire-format versioning is for code-evolution discipline, not user-compat.** P5 encrypted ciphertext flags (`0x01` → `0x02`), masking sentinel formats, etc. exist so dev/test databases can re-decrypt across runtime versions — not so deployed apps can be left alone.

**When this changes** (post-launch): this section gets a new "Migration discipline" subsection and `feedback_no_backward_compat.md` gets retired. Until then, treat back-compat constraints as user-requested guardrails only, never defaults.

---

## Where to start, by task

| If you're working on… | Start here |
| --- | --- |
| **Routing / dispatch / manifest** | `docs/architecture/gateway-routing.md` · `crates/zeroship-gateway/src/router/dispatch.rs` · `crates/zeroship-bundle/src/{manifest,rule}.rs` (`Manifest`, `Rule`, `Match`, `Action`) |
| **V8 runtime** (fetch, streams, WebSocket, modules) | `docs/architecture/runtime.md` · `crates/zeroship-runtime/` |
| **Adding a native primitive** (`env.*`) | `docs/reference/plugin-system.md` · `crates/zeroship-runtime-macros/` · `crates/plugin-{db,kv,storage}/` |
| **Control plane** (app CRUD, deploy, env, route registry) | `docs/architecture/control-plane.md` · `crates/zeroship-control/src/api.rs` · `crates/zeroship-control/src/registry.rs` |
| **Control-plane TypeScript client** (`@zeroship/control`) | `docs/reference/control.md` · `sdks/control/` · `crates/zeroship-control/src/{api,env_handlers}.rs` |
| **Deploy artifact** (.zship + manifest + blob storage) | `docs/reference/zship.md` · `docs/architecture/blob-store.md` · `crates/zeroship-bundle/` (manifest types, BlobStore, pack/unpack) |
| **Auth** (OIDC IdP + login UI + RPs) | `docs/reference/auth.md` · `crates/zeroship-auth/` · `crates/zeroship-gateway/src/oidc_rp.rs` · gates: `tests/run_auth_suite.sh` (live PG) + `tests/e2e_auth_ui.sh` (real Chromium against the real auth binary) |
| **How data is stored, reached and isolated** (databases, datastores, grants, schema authority) | `docs/architecture/data-system.md` - read this before changing anything in the data plane |
| **The DB SDK** (`@zeroship/db`) | `docs/reference/db.md` · `crates/zeroship-plugin-db/` |
| **The migration DSL** (`@zeroship/migrate`, portable op DSL) | `docs/reference/migrate-op-dsl.md` · `sdks/migrate/` · `crates/zeroship-schema/` · `crates/zeroship-migrate-server/` · `crates/zeroship-migrate*/` (the engine crates, in-sourced) · `db/migrations-ts/` (JS DSL; sole platform migration source — no SQL/Flyway) |
| **The PLATFORM's own schema** (`db/migrations-ts/`) | `deploy/ops/db-migrate.sh` (the sanctioned applier) · `tests/platform_migration_corpus_gate.sh` (proves it still applies) · `policies/platform.policy.toml`. It is authored in **`zero-migrate`**, the engine DSL — NOT `@zeroship/migrate`, and that is not an inconsistency. The corpus is applied by the engine's own Node CLI, which drains `zero-migrate`'s recorder; a file importing any other package records into a different ambient singleton and drains empty. It spelled `@zeroship/migrate` until 2026-08-28, which worked only because a since-deleted Rust binary aliased that name onto the engine bundle inside V8 — and left the platform unable to migrate its own database for as long as the alias was gone. Creator migrations still import `@zeroship/migrate`; the vite-plugin aliases it onto `zero-migrate` before recording (`sdks/vite-plugin/src/gen-types/recorder.ts:114-117`). |
| **The KV SDK** (`@zeroship/kv`) | `docs/reference/kv.md` · `sdks/kv/` · `crates/zeroship-plugin-kv/` |
| **The RPC SDK / server functions** (`@zeroship/rpc`) | `docs/reference/rpc.md` · `sdks/rpc/` · `sdks/vite-plugin/src/{transform,rpc-registry,manifest}.ts` · `sdks/bootstrap/src/dispatcher.ts` |
| **Durable workflows** (`@zeroship/workflows`, `env.workflows`) | `docs/reference/workflows.md` · `sdks/workflows/` · `crates/zeroship-plugin-workflow/` · `crates/zeroship-control/src/{workflow_instance_api.rs,cron/workflow_engine.rs}` · `crates/zeroship-worker/src/handler.rs` |
| **Build a creator app + deploy** (the primary creator flow) | `docs/build-and-deploy-golden-path.md` · `examples/starter/` (scaffold + `CLAUDE.md`) · `tests/golden_path.sh` · `crates/zeroship-cli/` (`zeroship deploy`) |
| **Creator project config** (`zeroship.jsonc`: app, control, build shape, migration paths, environments) | `docs/reference/project-config.md`, `schema/project-v1.json`, `crates/zeroship-cli/src/project_config/`, `sdks/vite-plugin/src/project-config/` |
| **zeroship deploy contract** (`default = { fetch?, rpc? }`, dispatcher, raw-JS deploys) | `docs/reference/zeroship-standard.md` · `sdks/bootstrap/src/{dispatcher,runtime-entry}.ts` · `crates/zeroship-runtime/src/core/init.rs` |
| **Framework-internal coordination** (`installSchema`, `__zsDispatch`, dev-entry) | `sdks/bootstrap/` · `sdks/bootstrap/README.md` |
| **Billing / metering / Stripe Connect** | `docs/reference/billing-metering.md` · `crates/zeroship-control/src/metering/provider/` · `crates/zeroship-stream/` · `crates/zeroship-control/src/cron/{event_forwarder,spend_recompute,billing_reconcile}.rs` |
| **WebSocket** (RFC 6455 implementation) | `docs/reference/websocket-design.md` · `crates/zeroship-runtime/src/` (search `WebSocket`) |
| **Vite plugin / build pipeline** (synthetic entry is a thin normaliser; runtime owns dispatch) | `docs/reference/vite-plugin.md` · `docs/reference/vite-environment-api.md` · `sdks/vite-plugin/src/rpc-registry.ts` |
| **Node.js compat** (npm packages in V8) | `docs/reference/node-compat.md` · `crates/zeroship-runtime/src/core/init.rs` |
| **Benchmarks** | `crates/zeroship-runtime/benches/` · `docs/reference/zerobench.md` · `docs/archive/benchmarks/` |
| **Local dev setup** | `docs/runbooks/local-dev.md` |
| **Multi-node / Docker Compose** | `docs/runbooks/docker-compose.md` |
| **Deploying to a remote server** (image-based, no source on the host) | `docs/runbooks/deploy-server.md` |
| **Sandbox / preview backend** (controller, in-VM agent, Nomad+Cloud-Hypervisor driver) | **Moved to the standalone `zeroship-sandbox` project** (sibling repo) — not built by this repo. The control plane reaches it over HTTP (`SANDBOX_URL`/`SANDBOX_TOKEN`); it uses this deployment's shared Postgres via the `sandbox_*` roles defined in `db/migrations-ts/20260702000100_schema_roles_extensions.ts` and granted in `db/migrations-ts/20260702000900_grants.ts`. |
| **Why we made decision X** | `docs/decisions/` (date-prefixed ADRs, immutable once landed) |
| **Pre-ship proposals** | `docs/proposals/` (active, may not have shipped) |
| **AI-builder competitive landscape** | `docs/research/ai-builder-features.md` |

---

## System map

Two systems, shared infrastructure, **zero tokio** — everything on compio/io_uring.

### System 1 — Creator Platform (creators build, deploy, operate)

```
Creator Dashboard (web UI)
  → Control Plane     App CRUD, deploy, billing, route registry
  → Builder Service   AI app generation, templates, preview
```

### System 2 — App Runtime (end users hit the apps creators ship)

```
End Users → Gateway          JWT, rate-limit, manifest dispatch, asset proxy, CHWBL routing
           → Auth Service    Login/signup, OAuth, consent, sessions
           → Workers (V8)    App code · env.{db,auth,kv,storage} primitives
                              (metering is infra: the primitives emit usage
                              metrics; there is no env.meter)
```

### Shared

```
PostgreSQL          One database, separate schemas (control, auth, per-app)
Object Storage      Bundles, assets, user uploads (LocalFs in dev; S3/R2 in prod)
DNS                 control · auth · api · {app} .zeroship.ai   (console is claimed but DEAD)
```

The authority is `deploy/ops/Caddyfile`, which declares five host blocks and nothing else:
`auth` -> `auth:9092`, `control` -> `control:9090`, `console` -> a console **this image does not
ship** (extracted in `8dbe4a8d4`), `api` -> `gateway:8000`, and `*` -> `gateway:8000` for creator-app
subdomains. A name appearing there is one a creator app may not register, and
`crates/zeroship-control/src/reserved_names.rs` is pinned to that file's sha256, so editing the edge
without regenerating fails `cargo test -p zeroship-control` rather than silently leaving the reserved
list describing an edge you replaced.

**This line listed `console · auth · {app}` until 2026-08-30** - omitting `control`, omitting `api`,
and leading with the one host that is dead. Read the Caddyfile, not this line, if the answer matters.

**`api.<domain>` is the gateway, and it is not a management API.** It serves nine endpoints, of which
exactly one is a proxy (`/__zeroship/internal/workflow-advance`, which forwards to a worker over the
hash ring). Seven terminate at the gateway - `/healthz`, `/readyz`, and the browser identity surface
under `/__zeroship/auth/*` that mints and re-signs session cookies - and the rest is app dispatch.
That hostname is also the gateway's `iss` claim (`--public-url` default `https://api.zeroship.ai`),
so it is a cryptographic identity, not just an address: repointing it moves end-user session issuance,
not a route.

### How they connect

```
Creator Platform                          App Runtime
────────────────                          ───────────
Control Plane ───deploy bundle──────────→ Object Storage
              ───update routes──────────→ Gateway (HTTP pull every 5s)
zeroship-migrate-server ─apply app migrations──→ PostgreSQL (per-app schema)

Auth Service ←──401 redirect────────────← Gateway (end-user not logged in)
             ───JWT cookie──────────────→ Gateway (validates per request)
             ───user profile────────────→ Workers (via ZeroShip-User header)
```

For the long form with sequence diagrams, see `docs/architecture/distributed.md`.

---

## Crate index

```
crates/
├── zeroship-core/    Inter-service wire types (RouteEntry, AppRecord, UsageReport, ControlEvent), typed_id, auth utils, observability
├── zeroship-bundle/  .zship deploy artifact: Manifest types, BlobStore, BundleStore, tar.zst pack/unpack
├── zeroship-schema/  Shared schema authority — DDL builders, diff classifier, live introspection, sentinel codec. Leaf (no v8/runtime); reused by the migration engine (write/diff) + plugin-db's data plane (read/introspect).
├── zeroship-migrate-server/ Managed-policy creator migration *service* — applies app migrations under the operator-ceiling ⊓ creator-draft trust profile. Its `session.rs` also carries `CompioPgSession`, the newtype bridging the `zeroship-migrate-*` engine crates to compio-postgres over their `SqlSession` seam. PostgreSQL only — it applies pure DDL and REFUSES anything else, including the SQLite rebuild step. The engine is multi-dialect; this host is not, and nothing here drives its MySQL or SQLite backends.
├── zeroship-runtime/ V8 + compio event loop + fetch + WebSocket + crypto + auth context
├── zeroship-runtime-macros/ #[v8_class] proc macro (V8 ObjectTemplate-backed classes)
├── zeroship-plugin-db/      env.db.* native ops
├── zeroship-plugin-kv/      env.kv.* native ops
├── zeroship-plugin-storage/ env.storage.* native ops
├── zeroship-metering/ Meter (atomic per-(app,metric) counters) + compio usage-event outbox task; NO V8. The data plugins emit usage metrics into it; there is no env.meter.
├── zeroship-stream/  Kafka-family durable event stream (StreamTransport trait + registry + Redpanda adapter)
│
│ System 1 — Creator Platform
├── zeroship-control/ Control plane (app CRUD, deploy, billing, env, route registry)
│
│ System 2 — App Runtime
├── zeroship-gateway/ Manifest dispatch, JWT, rate-limit, CHWBL routing, asset proxy
├── zeroship-worker/  V8-per-thread, on-demand bundle loading, LRU eviction
│
│ Tools
+-- zeroship-cli/     CLI: serve, deploy, migrate, config, login, logout, whoami, secret, var, dev
                      (no `build` — builds go through @zeroship/vite-plugin)
```

**Writing or changing a gate.** Every arm of every gate declares the number of
items THAT ARM RULED ON and a floor that number must clear
(`tests/lib/gate_arms.sh`; worked example `tests/ws_subscription_stub_gate.sh`).
This is not ceremony: on 2026-08-20 four gates were found to be examining
nothing and printing exactly what a clean tree prints, and a gate-level "3 arms
ran" guard was green throughout one of them because three arms did run, one over
an empty set. The floor lives beside the code that produces the number, never in
a central table - a table of expected counts is a census, and stale censuses are
how four OTHER gates went red the same week when two new crates landed.
`tests/gate_arm_census.sh tests` checks that every gate participates; add
`--run <gate.sh>` and it also rules on the counts those gates emit.

EVERY GATE IS A SHELL SCRIPT under `tests/`, and the census itself is one. Five
compose gates and the census were Rust in a `zeroship-gatekit` crate for a week;
all of it was deleted on 2026-08-21, the gates for the complexity they cost and
the census because 959 lines of Rust to read shell scripts and enforce a shell
convention is a workspace member paying for nothing. Write a new gate in shell,
source `tests/lib/gate_arms.sh`, and give every arm a floor.

Standalone, zeroship-independent driver libraries (own top-level `libs/`, publishable):

```
libs/
├── compio-postgres/  PostgreSQL driver (compio-native, replaces sqlx)
├── compio-redis/     Redis driver (cluster-aware, compio-native)
└── compio-s3/        S3 client (compio-native, hand-rolled sigv4)
```

Per-crate READMEs (where present) carry the responsibility statement and list of important files.

---

## Key invariants

These don't change. If you're about to violate one, stop and ask.

- **Zero tokio in the stack.** Everything is compio/io_uring. Drivers are bespoke (`compio-postgres`, `compio-redis`). The rule holds for code we write: no crate here declares tokio as a normal or build dependency, and every `tokio::` string in the tree is a comment saying what compio replaces. **A `[dev-dependencies]` tokio is ALLOWED, by an operator decision on 2026-08-24.** The invariant is that no tokio runtime drives our I/O in a shipped binary; a test binary is not shipped. It buys the strongest oracle a port can have - running tokio-postgres beside `compio-postgres` in one process and diffing their behaviour against the same server. The exemption is narrow and mechanically enforced: `kind == "dev"` only, declared in the member's own `[dev-dependencies]` with its own version. A normal or build dependency stays a hard red, and so does tokio in the root `[workspace.dependencies]`, which carries no kind and can be inherited into any table. Both directions are mutation-proved in `tests/zero_tokio_gate.sh`. It does NOT yet hold for the dependency graph - `cyper` pulls `hyper`, which pulls tokio, so `libtokio-*.rlib` is built (re-measured 2026-08-21 via `cargo tree -i tokio -e normal`: the third-party carriers are `cyper`, `cyper-core`, `hyper`, `hyper-util`, and nine of our crates name `cyper` directly - both sets unchanged since 2026-08-20). Removing that is the `investigate/cyper-tokio-removal` branch. **That edge is LINKED, NOT DRIVEN, and this paragraph used to omit it** - which is how a task was dispatched on 2026-08-21 to hand-roll an HTTP/1.1 client purely to avoid "adding a tokio edge" that was never a running runtime. No tokio reactor starts on our paths. `cyper` and `cyper-core` contain zero `tokio` occurrences in their own source (`grep -rc tokio ~/.cargo/registry/src/*/cyper{,-core}-*/src/*.rs`), and cyper-core supplies `CompioExecutor` (`hyper::rt::Executor` over `compio::runtime::spawn`) and `CompioTimer` (`hyper::rt::Timer` over `compio::time::sleep`), which `cyper::ClientBuilder::build` installs alongside its own `Connector` - so hyper's spawn, timer and connect hooks all land on compio and hyper-util's tokio-based `HttpConnector` is never constructed. hyper itself declares only `tokio = { features = ["sync"] }`, which needs no reactor. The `net`/`mio` features come from `hyper-util/client` naming `tokio/net` outright, NOT from hyper-util defaults (`default = []` is empty), so `default-features = false` would change nothing and the edge cannot be flagged away without dropping `hyper_util::client::legacy::Client` - which cyper uses. Empirically, a `cyper` GET returns `Ok(200)` inside a bare `#[compio::test]` runtime; a live path touching `tokio::net` or `tokio::time` would panic there instead (that exercises connect plus one request, not pool-idle timers). Read the pinned carrier sets as "compiled in", never as "a second runtime is running". The closure BEHIND those sets can shrink without either set moving, and did: `zeroship-gatekit` dropped its last `zeroship-core` dependency on 2026-08-21 (before the crate itself was deleted), taking the reachable count from 26 to 25. The gate was green either way, because gatekit reached tokio through core rather than by naming a carrier - so treat the two pinned sets as "has the accepted edge moved", never as a count of who is behind it. Do not read the exception as licence: adding a tokio-dependent crate still needs to be raised. **`tests/zero_tokio_gate.sh` now checks both halves** - it bans a non-dev tokio declaration in any manifest we own, and pins those two sets so the accepted edge cannot grow, shrink, or vanish without this paragraph changing in the same commit.
- **V8 per thread, one isolate per (app, live deploy) plus a bounded budget of pinned workflow isolates per app (`max_pinned_isolates_per_app`) for deploy-pinned workflow replay.** Worker uses LRU eviction; isolates `enter`/`exit` to allow many apps per thread (`crates/zeroship-worker/src/cache.rs`).
- **typed_id everywhere.** UUIDv7 + base62 + entity prefix (`usr_…`, `app_…`, `ses_…`). Defined in `crates/zeroship-core/src/typed_id.rs`.
- **Wire formats are explicit contracts.** `Manifest`, `RouteEntry`, `AppRecord`, `.zship` archive layout, and RPC envelopes must be changed deliberately. Pre-launch can break them, but every producer, consumer, fixture, and reference doc changes in the same patch; no hidden compatibility shim.
- **Native primitives are the kernel.** Anything user code can do via `fetch` or composition belongs in an npm package (`@zeroship/*`), not in Rust. The native surface is small and stable on purpose.
- **The gateway is dumb.** It does manifest dispatch, JWT, rate-limit, CHWBL routing — and forwards. All app logic runs in the worker.
- **Privilege follows the PROCESS, not the function.** The worker executes creator code. Granting the worker a privileged database capability - a `SECURITY DEFINER` wrapper, an elevated role, a signed session token it presents on its own behalf - does not create a boundary; it creates the *appearance* of one, because everything behind that capability is reachable by whatever reaches the worker. The rule that follows has two halves and both are load-bearing:
  - **If the worker can do it, it is not privileged.** It lives in the **app's own schema**, written by ordinary parameterised SQL, with provenance enforced at the Rust call boundary. No system schema, no `SECURITY DEFINER`, no session ceremony.
  - **If it must be privileged, it belongs to a separate service** that does not execute creator code - the migration service, the CDC relay, the control plane. Never to a function the worker calls.

  A system schema (`__zeroship_admin`) is therefore reserved for exactly one thing: **state a separate service WRITES and the worker only READS**, which the tenant must not be able to forge. A schema epoch is that shape. It is not a place to keep the worker's powers.

  **THAT SCHEMA DOES NOT EXIST TODAY, AND THIS PARAGRAPH SAID IT DID UNTIL 2026-08-29.** The sentence above describes the END STATE the invariant permits, not the tree. `__zeroship_admin`, its six tables and its 32 definer-rights routines were deleted on 2026-08-27 under this very invariant, and `crates/zeroship-plugin-db/src/auth/bootstrap.rs:15-18` records that **nothing replaced them**. `db/migrations-ts/` provisions no such schema - zero occurrences. One live statement still names it and therefore fails on every database: the PITR placeholder at `crates/zeroship-plugin-db/src/backend/postgres.rs:1286`, whose own comment at `:770-776` says the schema "NO LONGER EXISTS" and "the INSERT below therefore fails on every database", left in place because rehoming PITR targets is a design decision rather than a deletion.

  The schema epoch does not exist either - but **a comparison for it is built**. `crates/zeroship-plugin-db/src/transaction/reducer/identity.rs:97` defines `SchemaEpoch`, and `:313-315` compares observed against expected and returns `Verdict::ReResolve`. The producer is missing: `crates/zeroship-plugin-db/src/transaction/driver.rs:106` mints `SchemaEpoch::new(0)` on both sides, and says so - "The wiring is real; the *input* is not yet... the day a record exists, this is the one function that has to change."

  **THIS PARAGRAPH HAS NOW BEEN WRONG IN BOTH DIRECTIONS WITHIN ONE DAY, AND THE SECOND ERROR IS THE MORE INSTRUCTIVE ONE.** Earlier on 2026-08-29 it said the producer was the only missing piece. That was corrected the same morning to "necessary and not sufficient", on the grounds that a real `SET LOCAL ROLE` failure arrived as `BeginCompleted { opened: false }` and was routed to cleanup before `classify` ever saw it, so an epoch fence enforced by PostgreSQL needed a third piece nobody had named: an adapter from the session-setup outcome into `Verdict::ReResolve`.

  **That correction was itself outdated hours later, by a fix that landed the same day.** `8e191f650` replaced the `opened: bool` with a typed `BeginOutcome`, and the adapter now exists (`crates/zeroship-plugin-db/src/transaction/reducer/mod.rs:1005-1016`):

  ```rust
  BeginOutcome::SetupFailed => self.force(CleanupCause::SessionSetupFailed, now).1,
  BeginOutcome::ReResolve => self.on_verdict(Verdict::ReResolve, now),
  BeginOutcome::Denied(reason) => self.on_verdict(Verdict::Deny(reason), now),
  ```

  It is reachable, not merely spelled: `crates/zeroship-plugin-db/src/transaction/driver.rs:460-462` maps `SessionSetupDisposition::ReResolve` and its denial arm onto those outcomes, and both are covered at `crates/zeroship-plugin-db/src/transaction/reducer/tests.rs:1679` and `:1698`. `CleanupCause::BeginFailed` survives at `:1015` for a BEGIN that genuinely failed, which is what it was always for; it is no longer where classified setup errors go to die.

  So: the invariant is live and binding, the schema is a reservation rather than a fact, and **the epoch again needs only its producer** - the classifier, the adapter, the retryable verdict and both rotation directions all ship. Read the two sentences above as a design permission, never as a description of what you will find. And read the history of this paragraph as the standing warning it has earned: it has been rewritten twice in a day, each time correctly against the tree as it stood, and both times the tree moved underneath it. Verify these line numbers before you rely on them.

  **The counter-example is live in the tree.** DB-3: app JS reached a privileged unmask call and could pass `actor: { kind: "auto" }` to read its own PII, PHI and PCI at will. It is patched by `sanitize_app_actor`, which strips an actor claiming a reserved system kind to `None` (defined in `crates/zeroship-plugin-db/src/crud/unmask.rs`, applied at all three sites that reach `check_unmask_authorization`: `parse_args`, `parse_bulk_args`, and `crud/mod.rs`'s query-hint path) - but the bug is not an accident of that implementation. It is what the shape produces, and a privileged call the worker can make will keep producing it.

  Operator decision, 2026-08-27. Consequences already taken: the HMAC session anchor (`hmac_keys`, `session_ctx`, `session_nonces`, `sign_session`, `verify_signature`, `init_session`, `rotate_session_keys`) is deleted rather than completed; CDC slot and publication ownership moves to the CDC relay service rather than to a wrapper the worker calls.

---

## SDK layers

Two layers: native primitives (Rust kernel) and npm packages (JS ecosystem).

### Native primitives (`env.*` namespaces)

Registered native primitives appear as namespaces on the `env` object
(the 2nd arg to `fetch(req, env, ctx)` and the `env` named export of the
`zeroship` module). These are the platform "syscalls": small, stable,
low-level operations that SDK packages wrap.

Creator-facing namespaces registered today:

```
env.db.*       structured database operations (no raw SQL)
env.storage.*  object storage put/get/delete
env.kv.*       key-value get/set/delete
env.auth.*     getUser/requireUser — per-request identity (AuthPlugin, registered
               on the worker + CLI `zeroship serve` vectors). Fed in prod by the
               gateway's `ZeroShip-User` header; in dev by the dev-auth provider
               (see `docs/reference/auth-dev-tier.md`).
env.workflows.* durable workflow run start/control handles (`WorkflowPlugin`,
               registered on the worker + CLI `zeroship serve` vectors).
```

Planned or platform-internal namespaces must be documented as such until the
runtime actually registers them:

```
env.assets.*   runtime-emitted static asset CRUD (manifest runtime_assets)
```

Creators don't call these directly. SDK packages wrap them.

**Metering is infrastructure — there is NO `env.meter`.** The billing signal
is platform-measured so app code can neither forge nor suppress it: the worker
emits the five platform counters (requests/cpu_us/wall_us/ingress/egress) per
dispatch, and the trusted data primitives (`env.db`/`env.kv`/`env.storage`)
emit raw usage metrics (`db_reads`, `db_writes`, `kv_reads`, `kv_writes`,
`storage_ops`, `storage_bytes`, …) at their op boundary, in the success arm
only. The `Meter` + flush task live in `crates/zeroship-metering` (`MeterHandle` is the
per-app injection vehicle the plugins stamp from the server-injected `app_id`).

### SDK packages (`@zeroship/*` npm scope)

```javascript
import { env } from "zeroship";
import { auth } from "@zeroship/auth";
import { bucket } from "@zeroship/storage";
import { kv } from "@zeroship/kv";
import { Workflow } from "@zeroship/workflows";
import { query, mutation } from "@zeroship/rpc/server";

// Author schema changes as committed op.* migrations. The toolchain folds
// migrations into generated/zeroship/env.db.ts + schema.runtime.json, and
// runtime boot installs typed Collection wrappers on `env.db` from that fold.
// Handlers then write `env.db.users.find(...)` directly.
```

SDK packages call the `env.*` native primitives internally. Validation, query building, error mapping, TypeScript types all live in JS. They evolve independently of the Rust runtime.

### Framework-internal: `@zeroship/bootstrap`

`@zeroship/bootstrap` is the coordination package the runtime crate and Vite plugin both consume. It owns:

- `installSchema(schema, env.db, { descriptor })` — framework-internal installer for the generated RuntimeSchemaDescriptor
- `__zsDispatch` — the embedded RPC dispatcher (input parse / capability / stream framing)
- `normalizeUserModule` — namespace → `{ fetch, rpc, userDefault }` shape
- `createFetchHandler` — WinterCG fetch wrapper routing `/__zeroship/v1/<id>` through the dispatcher
- `runtime-entry.ts` — TLA orchestrator the runtime crate `include_str!`s
- `dev-entry.ts` — dev-mode equivalent the Vite plugin's dev-bootstrap delegates to

**User code MUST NOT import `@zeroship/bootstrap`.** It carries no back-compat guarantee; the runtime crate and Vite plugin are the only stable consumers. See `sdks/bootstrap/README.md`.

### When to add a native primitive vs. an npm package

| Needs Rust (new primitive) | Pure JS (npm package) |
| --- | --- |
| New storage backend | `@zeroship/email` (calls fetch) |
| New database engine | `@zeroship/payments` (Stripe wrapper) |
| Low-level crypto ops | `@zeroship/ai` (OpenAI/Claude wrapper) |
| New protocol (e.g., gRPC) | `@zeroship/permissions` (uses db + auth) |

Default to npm package. Native primitives are forever.

---

## Reference docs

Stable contracts, live in `docs/reference/`:

- `api-design-guidelines.md` — 10 principles for AI-friendly APIs
- `zeroship-standard.md` — the deploy contract: `default = { fetch?, rpc? }`, dispatch, raw-JS deploys
- `control.md` — `@zeroship/control`: framework-neutral client for control-plane app, auth, deploy, and env endpoints
- `db.md` — `@zeroship/db`: generated `env.db` typing, CRUD, aggregation, naming strategy
- `kv.md` — `@zeroship/kv`: ephemeral key-value surface, TTL, atomic counters, `setIfAbsent`, paginated `list`
- `rpc.md` — `@zeroship/rpc`: server wrappers, generated and manual clients, transport, transformers, retries
- `workflows.md` — `@zeroship/workflows`: replayable workflow classes, steps, sleeps, signals, children, schedules, outputs, and compensation
- `auth.md` — platform-managed auth, gateway JWT, OAuth, consent
- `auth-dev-tier.md` — the self-contained `pnpm dev` auth provider (the peer of `env.db`→SQLite / `env.kv`→redb): contract parity, the dev impl, the dev-only-by-construction guarantee
- `billing-metering.md` — Meter trait, 25+ metrics, pricing, spending limits
- `zship.md` — `.zship` deploy artifact format (tar.zst with content-addressed blobs)
- `websocket-design.md` — WebSocketPair, RFC 6455
- `plugin-system.md` — how to add an `env.*` namespace
- `node-compat.md` — Node.js module resolution in V8
- `project-config.md` - `zeroship.jsonc`: the creator project file the CLI and the
  build both read (app, control, build shape, migration paths, environments,
  secret names); its two precedence orders, the `config` escape hatch, and the
  scope invariant that keeps it out of the `.zship`
- `vite-plugin.md` — `@zeroship/vite-plugin`: node-compat shims, server-procedure discovery, the synthetic server entry, dev runtime + `.zship` build
- `vite-environment-api.md` — Vite dev server inside the V8 runtime
- `runtime-limits.md` — per-app `AppRuntimeLimits` vs runtime-side `RuntimeLimits` (CPU, wall timeout, heap), plus idle-GC knobs
- `sqlite-divergences.md` - intentional Postgres/SQLite differences in vector/spatial search, transaction isolation, locking, and text ordering
- `zerobench.md` — the HTTP/SSE/WS benchmark tool
- `env-vars.md` — every environment variable the tree reads, by service; the
  `ZEROSHIP_*` overlay vs bare CLI-flag naming families; the compose `.env`
  surface and which values are literals that ignore it

---

## Committing

A `commit-msg` hook enforces this. Turn it on once per clone, or your commits
are only checked in CI:

```bash
git config core.hooksPath .githooks
```

```
type(scope): imperative summary of what the change does
```

- `type` is exactly one of: `fix` `feat` `refactor` `test` `docs` `merge`
  `chore` `style` `build` `ci` `perf` `bench` `revert`. A scope is never a type
  — write `build(deploy): ...`, never `deploy: ...`.
- Exactly one lowercase scope, always present. Never `fix(a,b): ...`, never a
  bare `ci: ...`.
- Imperative and lowercase after the colon (`add`, `reject`, `keep`), no
  trailing period, **100 characters max**.
- No `#123`, no `3 of 8`, no `phase`/`stage`/`wave`/`milestone`. They are
  meaningless to anyone reading the log later; state the outcome instead.
- ASCII only. No em-dash, curly quote or arrow.
- Body only when the why is not obvious: prose, a few sentences, blank line
  after the subject, wrapped at 80 columns. A good subject carries most changes.
- **The whole message is capped at 500 characters.** With a ~75-char subject
  that is about six wrapped lines of body. Say what changed and why it is not
  obvious; measurements, transcripts and narration belong in the PR or a doc,
  not in `git log`.
- Breaking change: `!` before the colon (`refactor(core)!: ...`). No
  `BREAKING CHANGE:` footer.
- Every bug fix adds a regression test that would fail before the fix.

Full rules, scope vocabulary and worked examples: `CONTRIBUTING.md`. Check a
range yourself with `tests/commit_msg_gate.sh --range origin/main..HEAD`.

---

## Development

```bash
# Build (workspace) — build the SDKs FIRST. The runtime crate
# include_str!s `sdks/bootstrap/dist/{runtime-entry,install-schema}.js`
# and `sdks/db/dist/internal.js` (NOT dispatcher.js -- checked
# 2026-08-11; see the header of sdks/bootstrap/src/dispatcher.ts),
# so `pnpm build` must run before `cargo build -p zeroship-runtime`.
# Root `pnpm build` respects the dependency graph (bootstrap → db);
# cargo then sees the freshly emitted dist files.
#
# `sdks/vite-plugin` imports `zeroship-migrate-node`, a Rust N-API addon in
# the vendored engine whose outputs are untracked and which `pnpm install`
# does not build. Root `pnpm build` DOES build it -- it is the first
# filter in the chain (package.json, `pnpm --filter zeroship-migrate-node
# build && ...`), so one `pnpm build` on a clean checkout is enough and
# the addon needs Rust on PATH.
#
# THIS NOTE SAID THE OPPOSITE UNTIL 2026-08-11 -- that root `pnpm build`
# "filters to ./sdks/* and so NEVER builds it", with a separate
# `pnpm --filter zeroship-migrate-node build` line above the sequence. That
# was true when written and stopped being true in d4a5fcd5d ("fix(build):
# build the napi addon from the root pnpm build"), landed the same day.
# Nothing re-ran the note, so it kept reading as current. If you are
# about to add a manual addon step back, check package.json first.
#
# The original measurement still describes the FAILURE it protects
# against, and is worth keeping: remove only index.js / index.d.ts /
# *.node from third_party/zero-migrate/crates/zeroship-migrate-node and
# `pnpm --filter @zeroship/vite-plugin build` fails with
#   src/gen-types/addon.ts(66,8): error TS2307:
#       Cannot find module 'zeroship-migrate-node'
# restore them and the same command reports 0 errors. It is invisible on
# any machine that has already built the addon -- which is why the fix
# is that the ROOT build produces it rather than a step you must know.
pnpm build
cargo build --release

# Run single-tenant (dev)
zeroship serve myapp.js --port 3000

# Run platform (multi-node)
# Secrets are PATH flags: `--<name>-file`. There is no `--control-key <k>` and
# no `--db <dsn>` value flag; both were deleted. `zeroship dev init` writes the
# key material, and every flag below has a canonical ZEROSHIP_* env twin
# (docs/reference/env-vars.md).
zeroship-control --port 9090 --database-url-file ./secrets/control-dsn --blob-store ./bundles --control-key-file ./secrets/control-key
zeroship-worker --port 8080 --threads 16 --control-url http://localhost:9090 --control-key-file ./secrets/control-key --blob-store ./bundles
zeroship-gate    --port 80   --control-url http://localhost:9090 --control-key-file ./secrets/control-key --worker-urls http://localhost:8080 --blob-store ./bundles

# Deploy (a pre-built .zship artifact; auth via `zeroship login`, --token=<token>, or ZEROSHIP_TOKEN)
zeroship deploy ./dist/app.zship --app=<uuid> --control=http://localhost:9090 --token=<token>

# Docker Compose (all deploy config lives under deploy/)
docker compose -f deploy/compose/docker-compose.yml up -d --scale worker=10

# Tests (per crate)
cargo test -p zeroship-core
cargo test -p zeroship-gateway
cargo test -p zeroship-runtime --lib
cargo test -p compio-postgres -- --test-threads=1   # needs DB
# compio-postgres declares `default = []`, so THAT LINE IS FEATURE-BLIND: it
# skips every `tls`-gated test and reports a green that never compiled them.
# Measured 2026-08-27 at 04de4a91d: 428 lib tests on defaults, 456 under
# --all-features, and a fix to the rustls key-log path was verified against a
# run whose filter matched 0 tests and still printed "test result: ok". Add the
# feature-gated tests when a change touches anything behind a feature:
cargo test -p compio-postgres --features tls,live-tls-tests,live-unix-socket \
  -- --test-threads=1
# THAT FEATURE SET, NOT `--all-features`, IS THE ONE TO USE WITH `PG_TEST_URL`.
# `--all-features` also enables `suite-over-tls`, which `#[cfg]`-REPLACES
# `common::test_url()` so the whole suite reads its DSN from
# tests/data/live/tls_live.conf and IGNORES PG_TEST_URL entirely. Measured
# 2026-08-27 with PG_TEST_URL pointed at the 18.4 container: --all-features
# reported server_version_num=160015 (the fixture server), the feature set above
# reported 180004. A cross-version verdict was published off runs shaped like
# the former; both "versions" agreed perfectly because both were one server.
# Confirm the target rather than the variable:
#   cargo test -p compio-postgres --test suite -- --nocapture \
#     cancel_request::raw_cancel_interrupts_running_query_and_preserves_session
# prints `cancel oracle: server_version_num=... protocol=... backend_key_len=...`
#
# EITHER set TURNS ON TWO OPT-IN LIVE SUITES, and each needs its own fixture
# script to have run. They are written NOT to skip: a missing fixture is a loud
# failure naming the script, never a quiet pass.
#   tls_live (48 tests)         libs/compio-postgres/tests/tls_live_setup.sh
#   unix_socket_live (17 tests) libs/compio-postgres/tests/unix_socket_setup.sh
# With both present, measured 2026-08-27 at 04de4a91d: **1170 passed, 0 failed,
# exit 0** for the feature set above, and 1145 under --all-features (which runs
# a different, smaller set because suite-over-tls cfg-swaps some cases).
# With neither, the same command fails on fixtures alone and tells you nothing
# about the code.
#
# `tls_live_setup.sh` REGENERATES A SHARED CA, so it invalidates every other
# checkout's certs. Do not run it to clear a missing-fixture failure while
# another compio-postgres worktree exists - run those tests from a tree that
# already has them, or run the setup only once no other worktree is live.
#
# A `--lib --all-features` run does NOT build integration targets, so it cannot
# see either gap: that is how both suites stayed silently unrun for a whole
# session of otherwise-green --all-features checks.

# Lint the workspace. NONE of the per-crate runs above invoke clippy, which is
# why main went red twice in a week without anyone noticing. Run this before you
# push, not just before you wonder why CI is red.
#
# It is not a bare `cargo clippy --workspace`: a deny-level lint in one crate
# ABORTS the run before the crates downstream of it are ever scheduled, and a
# crate that was never reached prints exactly what a clean crate prints. The
# gate audits cargo's own json stream against `cargo metadata` and names any
# package or target that went unlinted. CI runs this same script.
#
# It lints under `--all-features`, and a fourth arm checks that every feature
# the manifests declare really came out enabled. That arm exists because the
# first three audit ONE feature resolution: a target whose `required-features`
# are unmet is not counted as unlinted, it is filtered out of the expectation,
# so the gate reported 148 of 148 on a workspace declaring 158. The 10 missing
# ones included zeroship-migrate-adapter's `platform_migrate`, which held eleven
# standing deny-level `clippy::await_holding_lock` errors the whole time.
#
# It needs `pnpm build` and setup-wpt.sh to have run (crates/zeroship-runtime
# `include_str!`s their output); it refuses, naming them, rather than linting a
# smaller workspace.
./tests/clippy_gate.sh
./tests/clippy_gate.sh --preflight-only   # "can this machine lint at all?" - seconds

# Web Platform Tests (WPT) — fetched on demand by setup-wpt.sh, NOT
# tracked in git. The script shallow-clones a pinned commit into
# crates/zeroship-runtime/tests/wpt/ (gitignored). The `crates/zeroship-runtime/tests/
# wpt_*.rs` runners `include_str!` upstream files verbatim (test
# files stay pristine — any shims/skips/sentinels live in the Rust
# runner code).
# After cloning the repo:
./crates/zeroship-runtime/tests/setup-wpt.sh                 # ~930 MB working tree at depth=1
# Bump the pin via WPT_COMMIT env var; default is the last-known-good
# commit baked into setup-wpt.sh. Re-run after pulling if the pin moves.

# E2E + benchmarks
./tests/e2e_platform.sh
./tests/bench_platform.sh
```

Detailed setup: `docs/runbooks/local-dev.md`. Multi-node: `docs/runbooks/docker-compose.md`.
Remote server (build locally, ship the image, no source on the host):
`docs/runbooks/deploy-server.md`.

---

## @zeroship/ui — Storybook MCP

When working on @zeroship/ui components, an MCP server is available at
`http://127.0.0.1:6006/mcp` while `pnpm --filter @zeroship/ui storybook`
is running. It exposes the standard Storybook MCP tools:

- `list-all-documentation` — every documented component
- `get-documentation` — props + examples for a specific component
- `get-story-documentation` — single-story detail
- Test Runner integration — execute the `play()` + a11y suite

Before referencing any @zeroship/ui prop in generated code, call
`get-documentation` to verify it exists. Do not assume props by
naming convention.

Tests for the UI library run via Storybook Test Runner:

```bash
pnpm --filter @zeroship/ui test-storybook          # against a running storybook
pnpm --filter @zeroship/ui test-storybook:ci       # boots http-server + runs runner
pnpm --filter @zeroship/ui test-storybook:coverage # + Istanbul report at sdks/ui/coverage/
```

Conventions for writing `play()` interactions and using
@storybook/test live in `sdks/ui/.storybook/CONVENTIONS.md`.

---

## Usage metering and payments

The platform meters the infrastructure each app consumes and provides a
Stripe-based payment integration. Two independent subsystems.

**Infrastructure usage metering (shipped).** Usage is measured server-side so
app code can neither forge nor suppress it. Pricing is a data-driven
plan catalog in the database (no operator API) — per tier: `base_fee`, `included_quota[metric]`,
`overage_rate[metric]`, `spend_limit_default`. Usage beyond the included quota
is metered as overage (the app keeps running, not blocked):
`charge = base_fee + Σ max(0, usage[m] − included[m]) × overage_rate[m]`. A
per-app configurable **spend limit** (not the quota) drives enforcement:
Warn (~80%) → Degrade (gateway throttle — tighter concurrency + rate limit, app
stays up) → Block (402 before dispatch). The free tier sets `spend_limit ≈ base`,
so it is quota-capped by construction and needs no card. The metered line items
are emitted as Stripe **invoice items** on a platform-side Customer (no
Stripe-side price objects). Live across the `metering` crate + the data
primitives (producers: worker platform counters + `env.{db,kv,storage}` usage
metrics) → `control` (idempotent ingest, aggregation, compute-unit pricing,
spend engine, Stripe reconciler) → `gateway` (edge enforcement). See
`docs/reference/billing-metering.md`.

**Payments (Stripe Connect).** Apps can accept payments from their end users via
Stripe **Connect**: the creator connects their own Stripe account and charges
settle to it. The platform can stamp a server-controlled fee on Connect charges,
enforced server-side so app code cannot bypass it (`FeePolicy { Fixed {
amount_cents } | Percent { percent, cap_cents?, floor_cents? } }`).
