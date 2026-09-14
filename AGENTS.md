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

## No statistics in durable artifacts

**Do not write magnitudes into docs, code comments, or commit messages** - byte counts, timings, percentages, test counts, tallies. A number in a durable artifact is a maintenance obligation nothing enforces, and prose does not re-measure itself. One proposal here needed repeated follow-up commits that changed no code, purely to repair its own figures.

- **State the shape, not the magnitude.** Which term dominates, and where the sign flips.
- **Name the instrument, not the value.** The setting or symbol a reader can look up stays true when its default moves; the number quoted for it does not.
- **Gate what is load-bearing.** A claim worth protecting is re-measured on every run, never asserted in prose.
- **A line number in a citation is the same defect.** The citation gate checks that a path resolves, never that a line is right, so drift stays invisible. Cite the path, name the function, and quote the code when the line matters.

None of this is licence to measure less - measure more, and put the result in a gate or a re-runnable script rather than a sentence. This file predates the rule and violates it throughout: fix the passage you are already touching, not the rest.

---

## Where to start, by task

| If you're working on… | Start here |
| --- | --- |
| **Routing / dispatch / manifest** | `docs/architecture/gateway-routing.md` · `crates/zeroship-gateway/src/router/dispatch.rs` · `crates/zeroship-bundle/src/{manifest,rule}.rs` (`Manifest`, `Rule`, `Match`, `Action`) |
| **V8 runtime** (fetch, streams, WebSocket, modules) | `docs/architecture/runtime.md` · `crates/zeroship-runtime/` |
| **Adding a native primitive** (`env.*`) | `docs/reference/plugin-system.md` · `crates/zeroship-runtime-macros/` · `crates/zeroship-{data,kv,storage}-v8/` |
| **Control plane** (app CRUD, deploy, env, route registry) | `docs/architecture/control-plane.md` · `crates/zeroship-control/src/api.rs` · `crates/zeroship-control/src/registry.rs` |
| **Control-plane TypeScript client** (`@zeroship/control`) | `docs/reference/control.md` · `packages/control/` · `crates/zeroship-control/src/{api,env_handlers}.rs` |
| **Deploy artifact** (.zship + manifest + blob storage) | `docs/reference/zship.md` · `docs/architecture/blob-store.md` · `crates/zeroship-bundle/` (manifest types, BlobStore, pack/unpack) |
| **Auth** (OIDC IdP + login UI + RPs) | `docs/reference/auth.md` · `crates/zeroship-auth/` · `crates/zeroship-gateway/src/oidc_rp.rs` · gates: `cargo xtask test auth` (owned PostgreSQL and service fixtures) + `tests/e2e_auth_ui.sh` (real Chromium against the real auth binary) |
| **How data is stored, reached and isolated** (databases, datastores, grants, schema authority) | `docs/architecture/data-system.md` - read this before changing anything in the data plane |
| **The DB SDK** (`@zeroship/db`) | `docs/reference/db.md` · `crates/zeroship-data-v8/` (adapter: V8 classes, per-isolate context, CDC) · `crates/zeroship-data-orm/` (engine: CRUD, transactions, exec, lanes) |
| **The migration DSL** (`@zeroship/migrate`, portable op DSL) | `docs/reference/migrate-op-dsl.md` · `packages/zero-migrate/` (the one authoring package and recorder) · `crates/zeroship-migrate-server/` · `crates/zeroship-migrate*/` (the engine crates, in-sourced) · `db/migrations-ts/` (JS DSL; sole platform migration source — no SQL/Flyway) |
| **The PLATFORM's own schema** (`db/migrations-ts/`) | `deploy/ops/db-migrate.sh` (the sanctioned applier) · `cargo xtask test migrations` (native corpus test in `crates/zeroship-migrate-node/tests/platform_corpus.rs`) · `policies/platform.policy.toml`. Platform and creator migrations both import the single **`@zeroship/migrate`** package in `packages/zero-migrate/`; the engine CLI and Vite plugin drain that package's one ambient recorder. This identity is load-bearing: importing a second implementation would record into another singleton and let the host drain empty. The 2026-08-28 outage was exactly that split; `docs/reviews/2026-08-28-migrate-dsl-fork-divergence.md` preserves the history. There is no alias and no second SDK package. |
| **Object storage and SDK** (`@zeroship/storage`) | `docs/reference/storage.md` · `crates/zeroship-storage/` (Rust operations) · `crates/zeroship-storage-v8/` (V8 binding) · `packages/storage/` |
| **KV storage and SDK** (`@zeroship/kv`) | `docs/reference/kv.md` · `crates/zeroship-kv/` (storage) · `crates/zeroship-kv-v8/` (V8 binding) · `packages/kv/` |
| **The RPC SDK / server functions** (`@zeroship/rpc`) | `docs/reference/rpc.md` · `packages/rpc/` · `packages/vite-plugin/src/{transform,rpc-registry,manifest}.ts` · `crates/zeroship-runtime/src/rpc/` |
| **Durable workflows** (`@zeroship/workflows`, `env.workflows`) | `docs/reference/workflows.md` · `packages/workflows/` · `crates/zeroship-workflow/` (Rust engine/client) · `crates/zeroship-workflow-v8/` (binding/executor) · `crates/zeroship-control/src/{workflow_instance_api.rs,cron/workflow_engine.rs}` · `crates/zeroship-worker/src/handler.rs` |
| **Build a creator app + deploy** (the primary creator flow) | `docs/build-and-deploy-golden-path.md` · `examples/starter/` (scaffold + `CLAUDE.md`) · `tests/golden_path.sh` · `crates/zeroship-cli/` (`zeroship deploy`) |
| **Creator project config** (`zeroship.jsonc`: app, control, build shape, migration paths, environments) | `docs/reference/project-config.md`, `schema/project-v1.json`, `crates/zeroship-cli/src/project_config/`, `packages/vite-plugin/src/project-config/` |
| **zeroship deploy contract** (`default = { fetch?, rpc? }`, dispatcher, raw-JS deploys) | `docs/reference/zeroship-standard.md` · `crates/zeroship-runtime/src/core/runtime_startup.rs` · `crates/zeroship-runtime/src/rpc/dispatch.rs` |
| **Framework-internal coordination** (startup, DB facade, dev entry loading) | `crates/zeroship-runtime/src/core/{runtime_startup,plugin_modules,dev_entry}.rs` · `crates/zeroship-data-v8/js/` · `packages/vite-plugin/src/dev-bootstrap/` |
| **Billing / metering / Stripe Connect** | `docs/reference/billing-metering.md` · `crates/zeroship-control/src/metering/provider/` · `crates/zeroship-stream/` · `crates/zeroship-control/src/cron/{event_forwarder,spend_recompute,billing_reconcile}.rs` |
| **WebSocket** (RFC 6455 implementation) | `docs/reference/websocket-design.md` · `crates/zeroship-runtime/src/` (search `WebSocket`) |
| **Vite plugin / build pipeline** (synthetic entry is a thin normaliser; runtime owns dispatch) | `docs/reference/vite-plugin.md` · `docs/reference/vite-environment-api.md` · `packages/vite-plugin/src/rpc-registry.ts` |
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
`auth` -> `auth:9092`, `control` -> `/v1/databases/*` to `migrate-server:9091` and everything else
to `control:9090`, `console` -> a console **this image does not
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

ORM structure and driver contracts: `docs/architecture/data-orm.md`.

```
crates/
├── zeroship-core/    Inter-service wire types (RouteEntry, AppRecord, UsageReport, ControlEvent), typed_id, auth utils, observability
├── zeroship-bundle/  .zship deploy artifact: Manifest types, BlobStore, BundleStore, tar.zst pack/unpack
├── zeroship-migrate-server/ Managed-policy creator migration *service* — applies app migrations under the operator-ceiling ⊓ creator-draft trust profile. Its `session.rs` also carries `CompioPgSession`, the newtype bridging the `zeroship-migrate-*` engine crates to compio-postgres over their `SqlSession` seam. PostgreSQL only — it applies pure DDL and REFUSES anything else, including the SQLite rebuild step. The engine is multi-dialect; this host is not, and nothing here drives its MySQL or SQLite backends.
├── zeroship-runtime/ V8 + compio event loop + fetch + WebSocket + crypto + auth context
├── zeroship-runtime-macros/ #[v8_class] proc macro (V8 ObjectTemplate-backed classes)
├── zeroship-data-v8/      env.db.* ADAPTER: V8 classes, per-isolate composition, service lifecycle and CDC. CRUD dispatch prepares and executes the engine's ORM operations, then encodes results for V8.
├── zeroship-data-orm/    ORM: bound Database and Collection handles, Rust model mapping, native values, SQL compilation, CRUD protection passes, transaction protocol, routed execution, transaction lanes and per-app role provisioning. The shared driver interface registers backend adapters. No V8 or adapter dependency.
├── zeroship-data-macros/    Migration-derived Rust collection metadata and FromRow, Insertable, Changeset derives. Re-exported through data-orm::orm; no runtime or driver dependency.
├── zeroship-kv/         App-scoped KV contract, errors, and Redis/redb backends; no V8
├── zeroship-kv-v8/      env.kv binding: V8 conversion, isolate state, dispatch, metering
├── zeroship-storage/    Scoped Rust object storage, LocalFs and S3 backends; no V8
├── zeroship-storage-v8/ env.storage binding, isolate-owned streams and metering
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
+-- zeroship-cli/     Creator CLI; run `zeroship --help` for available commands
```

**Database verification is required.** Worker tests own PostgreSQL containers
and run in ordinary `cargo test`. `cargo xtask test worker` builds the migration
host and runs the package. Control and migration-service tests still use
`tests/run_billing_suite.sh` to provision their databases and run the suites.
Database verification must never be an opt-in feature.
`zeroship-data-orm` and
`zeroship-data-v8` include PostgreSQL tests in ordinary `cargo test`. Do not
put required database cases behind opt-in features, ignore them, or report success
when the server or its required extensions are unavailable. Database contracts
live in their owning source crates, grouped by backend and behavior. Private
fixtures own test setup and teardown. `cargo xtask test data` runs the data crates
through nextest and rejects feature-gated test targets. Data fixtures own their
PostgreSQL containers; Docker is required and no external database URL is used.

**Writing or changing a check.** Keep nonempty-input assertions and rejection
controls beside each check. Do not add or port source-text checks: tests must
exercise behavior, compiler contracts, parsed artifacts or structured metadata,
rather than search implementation text for expected spellings. Retire existing
source scanners as their suites are migrated. Surviving shell gates use
`tests/lib/gate_arms.sh` for per-arm floors and failure propagation. There is no central script-count
census or requirement to recreate retired bookkeeping checks in Rust.

Data architecture checks are Rust tests in `xtask/tests/data_architecture.rs`,
run by `cargo xtask test data-architecture` and the complete data suite.
Workspace dependency and feature rules run through `cargo xtask test repository`
in `xtask/tests/repository_architecture.rs`. Driver and storage trait shape is
checked by the data architecture suite. The ORM and V8 adapter deny
`private_interfaces` and `private_bounds` during ordinary compilation.
Keep nonempty-input assertions and rejection controls beside the checks. Example acceptance
tests live inside each example and use Vitest, TypeScript fixtures and browser
assertions. Other repository gates remain shell scripts under `tests/` and
participate in `tests/lib/gate_arms.sh`.

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

- **Zero tokio in the stack.** Shipped I/O runs on compio/io_uring. Workspace
  members must not declare `tokio` or `tokio-*` as normal or build dependencies,
  including renamed and target-specific declarations. A member-owned
  `[dev-dependencies]` declaration with its own version is allowed for test
  oracles such as `tokio-postgres`. The root `[workspace.dependencies]` table
  must not declare these packages because its entries can be inherited into
  any dependency kind.

  The accepted transitive dependency through `cyper`, `cyper-core`, `hyper`, and
  `hyper-util` still compiles Tokio. Cyper installs compio executors, timers,
  and connectors; the dependency graph does not establish which runtime drives
  I/O. Adding a Tokio-dependent package still needs to be raised.

  Native tests in `xtask/tests/repository/tokio_boundary.rs`, run by
  `cargo xtask test repository`, enforce declarations and the accepted
  dependency boundary. Cargo selects normal dependency paths under default
  and all workspace features for the host platform. The tests compare their
  union against `CARRIERS` and inspect every dependency kind when comparing
  direct workspace consumers against `ENTRYPOINTS`. A new dev dependency on
  a carrier therefore changes the boundary even though direct Tokio dev
  dependencies are allowed. Internal dependency changes behind those direct
  consumers need not change either set.

  Update the sets and this invariant together when that accepted boundary
  changes, including when Tokio is removed. Rejection tests cover dependency
  aliases, target-specific kinds, TOML spellings, missing input, and optional
  feature activation. The workflow HTTP client belongs to `zeroship-workflow`;
  its accepted cyper dependency follows the Rust client.

- **V8 per thread, one isolate per (app, live deploy) plus a bounded budget of pinned workflow isolates per app (`max_pinned_isolates_per_app`) for deploy-pinned workflow replay.** Worker uses LRU eviction; isolates `enter`/`exit` to allow many apps per thread (`crates/zeroship-worker/src/cache.rs`).
- **typed_id everywhere.** UUIDv7 + base36 + entity prefix (`usr_…`, `app_…`, `ses_…`). Defined in `crates/zeroship-id/src/typed_id.rs`.
- **Wire formats are explicit contracts.** `Manifest`, `RouteEntry`, `AppRecord`, `.zship` archive layout, and RPC envelopes must be changed deliberately. Pre-launch can break them, but every producer, consumer, fixture, and reference doc changes in the same patch; no hidden compatibility shim.
- **Native primitives are the kernel.** Anything user code can do via `fetch` or composition belongs in an npm package (`@zeroship/*`), not in Rust. The native surface is small and stable on purpose.
- **The gateway is dumb.** It does manifest dispatch, JWT, rate-limit, CHWBL routing — and forwards. All app logic runs in the worker.
- **Privilege follows the process.** The worker executes creator code. A privileged
  database function the worker can invoke does not create a security boundary.
  Runtime writes belong in the app's schema under scoped, parameterized SQL.
  Privileged schema changes, replication ownership and key management belong to
  the migration service, CDC relay and control plane respectively.

  Every table in a bound creator schema is visible through the ORM and V8
  adapter. A table-name prefix does not change access, role grants or CDC
  publication. Schema binding remains the tenant boundary, and runtime code
  still cannot create schema objects. Any future shared system schema must hold
  state written by a separate service that workers cannot forge.

  Runtime descriptors define an isolate's schema. Catalog protection markers
  prevent descriptors from removing masking or encryption. Transaction identity
  checks support schema epochs, but `expected_authority` in
  `crates/zeroship-data-orm/src/transaction/driver.rs` still supplies a placeholder
  epoch; do not treat it as a live migration fence.

  Creator-supplied actors must pass through `sanitize_app_actor` in
  `crates/zeroship-data-orm/src/protection/unmask.rs`. Reserved system claims are
  removed from authorization and retained separately for audit. Unmask audit
  writes must survive rollback of the creator transaction.

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
env.workflows.* durable workflow run start/control handles (`WorkflowBinding`,
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

### Framework-internal coordination

`@zeroship/bootstrap` has been retired. The runtime owns startup readiness, entry validation,
procedure lookup and invocation, stream framing and subscription transport. The
Vite plugin emits procedure references for production and supplies ModuleRunner
entry snapshots in development; it does not invoke handlers.

`zeroship-data-v8` owns the host-only `installSchema` adapter and embeds its
compiled host module as `zeroship:db/adapter`. Creator imports cannot resolve
host modules. Native preparation supplies the DB
handle and validated descriptor before creator modules evaluate. The published
`@zeroship/db` package contains creator APIs and generated environment types;
it has no framework subpath. The SDK's `defineMaskPolicy` records a declaration
through `env.db.declareMaskPolicy`; the DB plugin installs and seals it during
native finalization. Vite emits no installer import or call.

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
# Build the JavaScript packages and host adapter before Cargo. The DB adapter embeds
# `crates/zeroship-data-v8/dist/adapter.js`; root `pnpm build` supplies it.
#
# `packages/vite-plugin` imports `zeroship-migrate-node`, a Rust N-API addon in
# `crates/zeroship-migrate-node` whose outputs are untracked and which `pnpm install`
# does not build. Root `pnpm build` DOES build it -- it is the first
# filter in the chain (package.json, `pnpm --filter zeroship-migrate-node
# build && ...`), so one `pnpm build` on a clean checkout is enough and
# the addon needs Rust on PATH.
#
# THIS NOTE SAID THE OPPOSITE UNTIL 2026-08-11 -- that root `pnpm build`
# "filters to the SDK package directory and so NEVER builds it", with a separate
# `pnpm --filter zeroship-migrate-node build` line above the sequence. That
# was true when written and stopped being true in d4a5fcd5d ("fix(build):
# build the napi addon from the root pnpm build"), landed the same day.
# Nothing re-ran the note, so it kept reading as current. If you are
# about to add a manual addon step back, check package.json first.
#
# The original measurement still describes the FAILURE it protects
# against, and is worth keeping: remove only index.js / index.d.ts /
# *.node from `crates/zeroship-migrate-node` and
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
# THAT SET IS STILL NOT ENOUGH WHEN A CHANGE TOUCHES THE TYPE CODECS. The
# `with-*` family gates TESTS as well as impls, and a gated-out test is not
# reported as skipped -- it simply is not in the binary. Measured 2026-08-27 at
# 6302fdee0: `libs/compio-postgres/tests/suite/temporal_edge_values.rs` declares 6 cases behind
# `with-chrono-0_4` / `with-time-0_3`; the set above compiled 3 of them and
# printed a clean green, and the three it dropped were exactly the ones proving
# that PostgreSQL `time '24:00'` is refused rather than silently aliased to
# midnight. Count the tests the file DECLARES against the ones that RAN:
#   grep -c '#\[compio::test\]' <file>   vs   `--list | grep -c <module>`
cargo test -p compio-postgres \
  --features tls,live-tls-tests,live-unix-socket,with-chrono-0_4,with-time-0_3 \
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

# Lint after building the SDKs and preparing WPT inputs.
# Workspace lint levels determine which diagnostics fail the command.
cargo clippy --workspace --all-targets --all-features

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
pnpm --filter @zeroship/ui test-storybook:coverage # + Istanbul report at packages/ui/coverage/
```

Conventions for writing `play()` interactions and using
@storybook/test live in `packages/ui/.storybook/CONVENTIONS.md`.

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
