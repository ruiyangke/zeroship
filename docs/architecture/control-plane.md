# Control plane

`crates/control` owns the creator API, auth endpoints, deploy ingest, env/secrets, and the registry feeds that gateway and worker poll.

There is no platform admin surface. The staff role table, the four staff Cedar
policies and every `/admin/*` route are deleted; what those routes managed is
either gone, deployment configuration read at boot, or creator self-service.

## HTTP surface

```text
/api/*      creator API
/auth/*     creator + end-user auth/session routes
/internal/* gateway/worker feeds and usage reporting
```

Current internal endpoints are:

- `GET /internal/routes`
- `GET /internal/versions`
- `GET /internal/apps/{app_id}`
- `GET /internal/apps/{app_id}/env`
- `GET /internal/apps/{app_id}/data-key`
- `POST /internal/usage`
- `POST /internal/webhooks/stripe`

Mutating `/api/*` endpoints are authorized per request through the Cedar
`AuthzGuard` against the caller OAuth access token. Worker and gateway reads
under `/internal/*` require service assertions and the endpoint allowlist in
`crates/zeroship-core/src/service_identity.rs` for gateway and worker feeds.
The project data-key endpoint permits worker identities; it does not expose
keys through the creator API or app environment.

## Module map

```text
main.rs            boot, config, route registration
lib.rs             `AppState`, shared services, secret wrappers
api.rs             app CRUD, deploy, plan, usage reads
internal.rs        route/version/env feeds, usage ingest
oauth_clients.rs   boot-time OAuth-client reconcile from `[auth] oauth_clients`
oauth_grants_handlers.rs  per-app OAuth grant management
authz_guard.rs     Cedar-backed request authorization (crates/authz)
egress_rules.rs    creator self-service raw-TCP egress rules
env_handlers.rs    vars/secrets CRUD + process.env exposure list
env_store.rs       encrypted-at-rest env/secrets storage
project_keys.rs    persistent project column keys and host delivery
secret_cipher.rs   shared control-owned secret wrapping keyring
registry.rs        PostgreSQL-backed app registry
stripe_handlers.rs Stripe-facing HTTP routes
stripe_store.rs    Stripe/account persistence
metering.rs        usage aggregation helpers
audit.rs           audit logging helpers
rate_limit.rs      control-plane rate limiting
http_util.rs       shared HTTP helpers
deploy.rs          re-exports `.zship` ingest limits/types from `zeroship_bundle`
```

## `AppState`

`crates/zeroship-control/src/lib.rs` wires these long-lived dependencies:

- `registry: Registry`
- `env_store: EnvStore`
- `stripe_store: StripeStore`
- `auth: AuthService`
- `google_oauth: Option<GoogleConfig>`
- `vfs: Arc<dyn BundleStore + Send + Sync>` currently backed by `zeroship_bundle::LocalFs`
- `blob_store: Arc<dyn BlobStore>` currently backed by `zeroship_bundle::LocalDiskBlobStore`
- secret-bearing config wrapped in `SecretString`
- admin + webhook rate limiters
- `deploy_tmp_dir` for streamed `.zship` uploads

The `apps` table currently carries the routing/deploy state the rest of the platform consumes:

```sql
apps(
  id uuid primary key default gen_random_uuid(),
  name text not null unique,
  plan_id text not null default 'free',
  deploy_hash text,
  env_version bigint not null default 0,
  manifest_json text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
)
```

There is NO app-level API key here, and that is a decision rather than an
omission: `db/migrations-ts/20260905000200_drop_app_api_key.ts` records why the
platform does not own one, and what shape long-lived programmatic access would
have to take if it becomes a feature.

The schema is owned by the platform migration corpus (`db/migrations-ts/`,
especially `20260702000200_control_tables.ts`) and applied by
`zeroship-platform-migrate`; the registry consumes these tables but does not
create them.

## Route and version feeds

`Registry::get_gateway_snapshot()` builds the gateway feed: a
`RouteMap<Uuid, RouteEntry>` plus lifecycle rows for disabled, anonymized, or
deletion-pending principals. The gateway uses that pushed state to invalidate
stateless app credentials without a per-request database lookup. If
`manifest_json` is missing, unparsable, or fails `Manifest::validate()`, the
registry falls back to `Manifest::passthrough()` and logs an error.

`Registry::get_versions()` builds `VersionMap<Uuid, AppVersionInfo>` for workers. The manifest in that feed is optional, so undeployed apps can still appear in the version map with `manifest = None`.

Gateway polls `/internal/routes` every 5 seconds. Worker polls `/internal/versions` every 5 seconds and fetches env snapshots lazily from `/internal/apps/{app_id}/env` when `env_version` changes.
These feeds are polled rather than pushed so the control plane stays stateless with respect to gateway and worker consumers.

Before loading an app with a database service, the worker fetches its project
column key from `/internal/apps/{app_id}/data-key` into the host's shared
`SuppliedProjectKeys`. Control resolves the app's project from the registry and
serializes initial provisioning by locking that project. The wrapped key lives
in `zeroship.project_data_keys`, accessible only to control's database role.
Wrapping-key rotation preserves the data key. App environments, runtime
descriptors, and bundles contain no key material. Standalone local development
persists a separate key in `.zeroship/private/project-data-key.json`.

## Deploy ingest

A deploy is a command with a client-minted identity (`dcm_...`), and the
handler is `api::deploy`:

```text
1. Before any body byte is read: authz (`AppsDeploy` on `Resource::App{id}`),
   content type (only `application/x-zship`), exactly one canonical
   `Idempotency-Key` naming the deploy command, and a live app.
2. The body is streamed to a temp file under `deploy_tmp_dir`, bounded by
   `MAX_COMPRESSED_BYTES`, and hashed. The receipt binds that digest, never a
   hash the caller claims.
3. A receipt for the command id answers here: an exact repeat (same app,
   actor, content type and digest) returns the stored acceptance, anything
   else is 409 `idempotency_key_conflict`.
4. `zeroship_bundle::ingest(...)` streams `blobs/<hash>` through
   `BlobStore::put_blob_stream` and writes `manifests/<app_id>/<deploy_hash>.json`.
5. Declared OAuth scopes are validated, the manifest's workflow schedules are
   projected and checked against the scheduler's bounds
   (`publication::VerifiedDeployment`), and the per-app OAuth client is
   reconciled.
6. One Control catalog transaction (`publication::catalog::accept`): schema
   admission against the app's newest applied migration, the `app_deploys`
   row, the app's current deployment pointer, the next lifecycle revision with
   an activation intent (unless the app is archived), and the command receipt.
```

Any refusal or failure in step 6 rolls back every write in it. Deploy never
applies migrations: `zeroship migrate` applies them through the migration
service, and a deploy whose runtime descriptor differs from the app's newest
applied migration is refused with 409 `schema_not_applied`.

Deploy, archive and restore run their catalog transactions on the catalog the
whole process shares (`publication::Catalog`) instead of opening a connection
each. An ORM database belongs to the compio thread that opened it, and that
thread admits one top-level transaction per binding at a time, so the catalog
is `control.catalog_max_connections` threads holding one session each: the
bound is both the sessions the process holds and the catalog transactions that
run at once, and operations beyond it wait for a free thread. Each operation
keeps its own lock order and single transaction, and a caller that stops
waiting cancels its transaction as before. Catalog sessions announce
themselves as `zeroship-control-catalog` in `pg_stat_activity` unless the
database URL names a session already.

The workflow manager learns of an accepted deployment asynchronously.
`publication::publisher` pages pending lifecycle intents, delivers each app's
intents in revision order through the manager's signed schedule routes, and
records only the manager's exact receipt in a fresh transaction. Archive and
restore commit disable and activation intents through the same catalog, and the
deployment collector keeps a pending activation's bundle until the manager's
queue hold takes over.

The blob-store ingest path is current. The older raw bundle upload path is gone.

## Auth flow

End-user auth does **not** terminate in control. The gateway is the OIDC
RP of the native auth service (`crates/auth`); control is a pure API
resource server with no RP of its own — the bespoke `ConsoleOidcRp` +
`console_sessions` surface was removed in the R5 cutover
(`crates/zeroship-control/src/lib.rs`).

```text
Gateway -> 302 to auth /oauth2/authorize (no session cookie)
crates/auth -> login / OAuth / consent, then native OP code issuance
Gateway -> /__zeroship/auth/callback: code exchange, sets `__Host-zeroship_app_session`
Gateway -> validates the session and forwards `ZeroShip-User` (HMAC-signed)
Worker/runtime -> reads the forwarded user context
```

See `docs/reference/auth.md` for the full flow.

## Notes

- `EnvStore` uses `zeroship_core::crypto` for encrypted-at-rest secrets, with primary + previous master-key support for rotation.
- `BundleStore` is still present on `AppState`, but deploy ingestion and runtime asset serving use `BlobStore`.
- Stripe support lives in `stripe_handlers.rs` and `stripe_store.rs`; worker metering still arrives through `/internal/usage`.

## Related docs

- [docs/architecture/overview.md](../architecture/overview.md) — Platform entry point and system map
- [docs/architecture/distributed.md](../architecture/distributed.md) — How feeds propagate across nodes
- [docs/architecture/gateway-routing.md](../architecture/gateway-routing.md) — Gateway-side route resolution and dispatch
- [docs/architecture/blob-store.md](../architecture/blob-store.md) — Bundle and asset blob storage architecture
- [docs/reference/auth.md](../reference/auth.md) — Creator and end-user authentication model
- [docs/reference/billing-metering.md](../reference/billing-metering.md) — Usage reporting, metering, and billing flows
