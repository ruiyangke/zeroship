# Control plane

`crates/control` is the single creator-facing API and the registry that the gateway/worker poll. Everything the creator does — create app, deploy, manage env, configure auth, view billing — flows through here.

## Surface

Two HTTP roots:

```
/api/*        Creator-facing. Bearer master-key gates create/delete/deploy.
/internal/*   Gateway/worker-facing. Bearer control-key gates everything.
```

Plus a few unauthenticated routes for end-user OAuth callbacks.

## Module map (`crates/control/src/`)

```
main.rs            Wires routes, payload caps (16 MB on /deploy + /assets/...), TLS, logger
lib.rs             AppState, shared dependencies (vfs, env_store, registry, oauth)
api.rs             /api/apps/* CRUD + deploy + asset upload
internal.rs        /internal/* — health, env fetch, asset GET, usage report
auth_handlers.rs   /auth/* creator and end-user routes (login, signup, OAuth start/callback)
auth_service.rs    Cookie session, password hashing, JWT mint
oauth.rs           Pluggable OAuth provider (Google today; trait-driven)
env_handlers.rs    /api/apps/{id}/env — vars + secrets CRUD
env_store.rs       Encrypted-at-rest secret storage (libsodium)
stripe_handlers.rs /api/stripe/* — Stripe Connect onboarding, webhooks
stripe_store.rs    Persistence for connect accounts, payouts ledger
metering.rs        Aggregate workers' UsageReports → per-app counters
audit.rs           Audit log for sensitive ops
rate_limit.rs      Control-plane self-throttling (separate from gateway's per-app limits)
http_util.rs       Shared HTTP helpers
registry.rs        Postgres-backed app/route registry. Builds the RouteMap for the gateway.
```

## State and persistence

`AppState` (`lib.rs`) carries:

- `vfs: Arc<dyn BundleStore>` — `LocalFs` in dev, S3/R2 in prod (the trait is in `crates/core/src/vfs.rs`). Stores bundles + assets.
- `env_store: EnvStore` — Postgres + libsodium-encrypted secret storage.
- `registry` — direct Postgres handle for the apps table.
- `oauth` — optional Google OAuth config; `None` disables `/auth/google/*`.
- `control_key` (Zeroizing String) — bearer token gating internal routes.

The `apps` table schema (managed by `registry.rs`):

```sql
apps(
  id            uuid primary key,
  name          text unique,
  plan_id       text,
  api_key       text,
  api_key_hash  text,
  deploy_hash   text,
  manifest_json text,           -- the per-app routing manifest
  env_version   bigint default 0,
  created_at    timestamptz default now(),
  updated_at    timestamptz
)
```

`manifest_json` is the wire-format JSON of `Manifest`. NULL → `Manifest::passthrough()` is synthesized at registry-load time.

## RouteMap synthesis

`registry::list_routes()`:
1. Reads all apps from Postgres.
2. For each row, parses `manifest_json` and runs `Manifest::validate()`. Validation failure → drop the manifest (use passthrough), log a warning.
3. Builds a `RouteMap = HashMap<Uuid, RouteEntry>` and returns.

The gateway polls `/internal/routes` every 5s. `crates/gateway/src/sync.rs::sync_once` ingests this, compiles each `Manifest` into its `CompiledManifest` form, and atomically swaps the gateway's route cache.

## Deploy path

```
1. Creator → POST /api/apps/{id}/deploy  (16 MB body cap)
   Body: .appbundle bytes (binary) OR raw ES module bytes (text/javascript fallback)

2. api::deploy:
   - Verify master key
   - Compute deploy_hash = sha256(body)
   - vfs.put(app_id, body)
   - UPDATE apps SET deploy_hash = ..., updated_at = now()

3. (Future) Build adapter writes manifest_json alongside the deploy.

4. Worker polls /internal/routes every 5s
   - Sees deploy_hash changed for this app
   - Fetches new bundle bytes via /internal/bundle/{app_id}
   - Reloads V8 isolate (cache.rs::load_app)
```

## End-user auth flow

```
End user → app.zeroship.ai/protected
  → Gateway: no JWT cookie → 302 to control's /auth/authorize?app_id=...&return=/protected
  → Control: shows login UI (or kicks off OAuth)
  → Control: on success, sets __zs_session cookie scoped to *.zeroship.ai with HS256 JWT
  → User's browser: redirects back to app.zeroship.ai/protected
  → Gateway: validates JWT, injects ZeroShip-User header, forwards to worker
  → Worker: zeroship.auth.getUser() reads the header, returns the user object
```

Detailed: `docs/reference/auth.md`.

## Billing flow

```
Worker → POST /internal/usage every N seconds
   Body: UsageReport { worker_id, counters: { app_id → AppUsage { requests, cpu_us, ... } } }

Control:
   metering.rs aggregates per app
   stripe_store records billable events
   stripe_handlers webhooks reconcile against actual charges
```

Detailed: `docs/reference/billing-metering.md`.

## Operational notes

- **Master key vs control key.** Creator-facing ops (`/api/*`) check the master key. Gateway/worker-facing ops (`/internal/*`) check the control key. Different surfaces, different secrets — leaking one doesn't leak the other.
- **`get_app` admin gate.** `GET /api/apps/{id}` surfaces the `api_key` only when admin-authed. Anonymous reads get the public `AppRecord` without the secret. Lets the dashboard show the key to the owner without leaking it on every public lookup.
- **`insecure_dev` flag.** Disables `/internal/*` auth checks. Use for local dev only — never set in prod.

## Where to start by sub-task

| Working on… | Read first |
| --- | --- |
| Adding a creator-facing endpoint | `crates/control/src/api.rs` (existing handlers as templates) |
| Adding a gateway-facing endpoint | `crates/control/src/internal.rs` (note `check_auth` pattern) |
| Auth provider | `crates/control/src/oauth.rs` (trait); add a new impl |
| Schema migration | `crates/control/src/registry.rs` (look for `ALTER TABLE` blocks at startup) |
| Stripe integration | `crates/control/src/stripe_*.rs`; webhook handler in `stripe_handlers.rs` |
| Encryption / secrets | `crates/control/src/env_store.rs` (libsodium-sealed boxes) |
