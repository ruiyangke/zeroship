# Control plane

`crates/zeroship-control` owns the creator API, auth endpoints, deploy ingest, env/secrets, and the registry feeds that gateway and worker poll.

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
- `GET /internal/apps/{app_id}/binding`
- `POST /internal/workers/join`, `POST /internal/workers/renew`, `POST /internal/workers/retire`
- `POST /internal/billing/reconcile`
- `POST /internal/spend/reconcile`
- `POST /internal/webhooks/stripe`

`erasure`, `deployment_hold_api`, and `health` register further `/internal/*`
routes through `configure`.

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
metering/           usage aggregation and role-addressed billing providers
audit.rs           audit logging helpers
http_util.rs       shared HTTP helpers
deploy.rs          re-exports `.zship` ingest limits/types from `zeroship_bundle`

`rate_limit.rs` no longer exists: control-plane rate limiting is
`zeroship_authn::rate_limit`, re-exported through `lib.rs`. The control crate
has also grown modules the map above predates — organizations and billing
(`organizations.rs`, `billing_read.rs`, `cron/`, `pricing*.rs`, `spend.rs`,
`openmeter_client.rs`, `tax.rs`), deploy publication (`publication/`,
`deploy_inflight.rs`, `deployment_hold_api.rs`), worker instances
(`worker_join.rs`, `join_minter.rs`, `worker_health.rs`), and account/erasure
(`erasure.rs`, `device_handlers.rs`, `account_status.rs`).
```

## `AppState`

`crates/zeroship-control/src/lib.rs` wires these long-lived dependencies:

- `registry: Registry`
- `env_store: EnvStore`
- `stripe_store: StripeStore`
- `blob_store: Arc<dyn BlobStore>` currently backed by `zeroship_bundle::LocalDiskBlobStore`
- secret-bearing config wrapped in `SecretString`
- admin + webhook rate limiters
- `deploy_tmp_dir` for streamed `.zship` uploads

The former `auth: AuthService`, `google_oauth: Option<GoogleConfig>`, and
`vfs: ... BundleStore` fields are gone. `AppState` now also carries the
coordination, authorization, and billing dependencies the deploy-publication
and metering paths need: `service_auth`, `control_pg`, `auth_provider`,
`static_policies`, `provider_registry`/`billing_stack`, `tax_provider`,
`notifier`, `mailer`, `worker_enrolment`, and `pairwise_salt`.

The `apps` table currently carries the routing/deploy state the rest of the platform consumes:

```sql
apps(
  id text primary key,
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
`SuppliedProjectKeys`, and its resolved database binding from
`/internal/apps/{app_id}/binding` into the host's shared `SuppliedAppBindings`.
The binding response carries the database id, the edge id and the schema epoch;
the worker composes no part of it, and Control serves only a binding whose
status is active and whose `observed_generation` has caught up to its
`generation`. An app Control serves no live binding for has no `env.db`.
Both reads happen once per app per worker process. The key is one value for the
life of the app. The binding carries the schema epoch, which an apply that
commits a schema delta advances, and the epoch is part of the binding role
name - so a worker holding the epoch it first resolved composes a role the
apply after next drops, and every session that app opens is then refused at
`SET LOCAL ROLE`. The binding is deliberately not re-read on a bare environment
refresh: live isolates consult the store at each `env.db` call, so carrying a
rotation into it under a resident isolate would let code built against an older
shape succeed against the schema that replaced it, which is the direction the
epoch fence exists to catch. Resolving the epoch belongs with replacing the
isolate, and nothing does that today.

Control resolves the app's project from the registry and serializes initial
provisioning by locking that project. The wrapped key lives
in `zeroship.project_data_keys`, accessible only to control's database role.
Wrapping-key rotation preserves the data key. App environments, runtime
descriptors, and bundles contain no key material. Standalone local development
persists a separate key in `.zeroship/private/project-data-key.json` and its own
binding in `.zeroship/private/dev-database-binding.json`; both are minted on
first start and never replaced, because a new database id would abandon the
schema the existing rows are in.

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
5. Declared OAuth scopes are validated and the manifest's workflow schedules
   are projected and checked against the scheduler's bounds
   (`publication::VerifiedDeployment`).
6. If the manifest declares any workflow, `publication::DeployJournal` asks the
   manager to bring that app's creator-database journal to the current version
   (`endpoints::WORKFLOW_JOURNAL_ENSURE`). It is idempotent, so the deploy path
   calls it every time and Control keeps no record of having done so. A refusal
   ends the deploy with 503 `workflow_journal_unavailable` before anything is
   committed; the per-app OAuth client is then reconciled.
7. One Control catalog transaction (`publication::catalog::accept`): schema
   admission against the app's newest applied migration, the `app_deploys`
   row, the app's current deployment pointer, the next lifecycle revision with
   an activation intent (unless the app is archived), and the command receipt.
```

The journal is ensured before that transaction on purpose: the activation
intent it commits is what tells the manager the deployment exists, so a journal
provisioned after it would leave a window in which a run is accepted against a
journal that is absent or behind. A failure fails the deploy rather than being
queued for retry, because the creator is waiting on the answer and an exact
resend under the same `Idempotency-Key` repeats the whole path.

Any refusal or failure in step 7 rolls back every write in it. Deploy never
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
`publication::publisher` runs on the shared catalog, pages pending lifecycle
intents, delivers each app's intents in revision order through the manager's
signed schedule routes, and records only the manager's exact receipt in a
fresh transaction. An app whose attempt fails waits out a retry delay that
doubles to a cap and resets after a success, while other apps keep publishing.
Archive and restore commit disable and activation intents through the same
catalog, and the deployment collector keeps a pending activation's bundle
until the manager's queue hold takes over.

Control refuses to start when publication could not run: without its service
key, or with a `control.workflow_coordinator_url` the manager client refuses.
The origin is checked with the rest of the configuration, so `--check-config`
refuses it too. The service key is loaded before the process touches the
database. Tests that need no publisher build `AppState` directly and never
start one.

The blob-store ingest path is current. The older raw bundle upload path is gone.

## Auth flow

End-user auth does **not** terminate in control. The gateway is the OIDC
RP of the native auth service (`crates/zeroship-auth`); control is a pure API
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
- `BundleStore` is gone from `AppState`; deploy ingestion and runtime asset serving both use `BlobStore`.
- Stripe support lives in `stripe_handlers.rs` and `stripe_store.rs`; worker metering is emitted through the `zeroship_metering` usage outbox and reconciled through the billing stream, not `POST /internal/usage`.

## Related docs

- [docs/architecture/overview.md](../architecture/overview.md) — Platform entry point and system map
- [docs/architecture/distributed.md](../architecture/distributed.md) — How feeds propagate across nodes
- [docs/architecture/gateway-routing.md](../architecture/gateway-routing.md) — Gateway-side route resolution and dispatch
- [docs/architecture/blob-store.md](../architecture/blob-store.md) — Bundle and asset blob storage architecture
- [docs/reference/auth.md](../reference/auth.md) — Creator and end-user authentication model
- [docs/reference/billing-metering.md](../reference/billing-metering.md) — Usage reporting, metering, and billing flows
