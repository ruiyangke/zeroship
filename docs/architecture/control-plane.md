# Control plane

`crates/control` owns the creator/admin API, auth endpoints, deploy ingest, env/secrets, and the registry feeds that gateway and worker poll.

## HTTP surface

```text
/api/*      creator/admin API
/auth/*     creator + end-user auth/session routes
/internal/* gateway/worker feeds and usage reporting
```

Current internal endpoints are:

- `GET /internal/routes`
- `GET /internal/versions`
- `GET /internal/apps/{app_id}`
- `GET /internal/apps/{app_id}/env`
- `POST /internal/usage`
- `POST /internal/webhooks/stripe`

Mutating `/api/*` endpoints accept either an authenticated admin session or the master-key bearer. `/internal/*` is gated by the control-key bearer unless `--dev-insecure` is enabled.
The master-key is the human or automation credential for creator/admin control-plane mutations, while the control-key is the machine-to-machine bearer gateway and worker use for `/internal/*` feeds and usage reporting.

## Module map

```text
main.rs            boot, config, route registration
lib.rs             `AppState`, shared services, secret wrappers
api.rs             app CRUD, deploy, plan, usage reads
internal.rs        route/version/env feeds, usage ingest
token_handlers.rs  PAT issuance (`/me/tokens`)
oauth_handlers.rs  admin OAuth-client CRUD against hydra
oauth_grants_handlers.rs  per-app OAuth grant management
authz_guard.rs     Cedar-backed request authorization (crates/authz)
admin_handlers.rs  platform-admin surface
bootstrap_console.rs  R5 console seed (`--bootstrap-console`)
env_handlers.rs    vars/secrets CRUD + process.env exposure list
env_store.rs       encrypted-at-rest env/secrets storage
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

`crates/control/src/lib.rs` wires these long-lived dependencies:

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
  api_key text not null,
  api_key_hash text not null default '',
  env_version bigint not null default 0,
  suspended boolean not null default false,
  audit_locked boolean not null default false,
  manifest_json text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
)
```

The schema is owned by zeroship-migrate (`db/migrations/`,
`V0004__control.sql`); the registry consumes these tables but does not create
them.

## Route and version feeds

`Registry::get_routes()` builds `RouteMap<Uuid, RouteEntry>` for the gateway. If `manifest_json` is missing, unparsable, or fails `Manifest::validate()`, the registry falls back to `Manifest::passthrough()` and logs a warning.

`Registry::get_versions()` builds `VersionMap<Uuid, AppVersionInfo>` for workers. The manifest in that feed is optional, so undeployed apps can still appear in the version map with `manifest = None`.

Gateway polls `/internal/routes` every 5 seconds. Worker polls `/internal/versions` every 5 seconds and fetches env snapshots lazily from `/internal/apps/{app_id}/env` when `env_version` changes.
These feeds are polled rather than pushed so the control plane stays stateless with respect to gateway and worker consumers.

## Deploy ingest

```text
1. `api::deploy` accepts only `application/x-zship`.
2. The request body is streamed to a temp file under `deploy_tmp_dir`.
3. `zeroship_bundle::ingest(...)`:
   - opens the tar.zst
   - requires `manifest.json` first
   - validates `Manifest.version == 1`
   - streams `blobs/<hash>` through `BlobStore::put_blob_stream`
   - writes `manifests/<app_id>/<deploy_hash>.json`
4. Control updates `apps.deploy_hash` and `apps.manifest_json`.
```

The blob-store ingest path is current. The older raw bundle upload path is gone.

## Auth flow

End-user auth does **not** terminate in control. The gateway is the OIDC
RP of the auth service (`crates/auth` + hydra); control is a pure API
resource server with no RP of its own — the bespoke `ConsoleOidcRp` +
`console_sessions` surface was removed in the R5 cutover
(`crates/control/src/lib.rs`).

```text
Gateway -> 302 to hydra /oauth2/auth (no session cookie)
crates/auth -> login / OAuth / consent, accept_login against hydra
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
