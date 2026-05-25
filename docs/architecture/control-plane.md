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
- `GET /internal/env/{app_id}`
- `POST /internal/usage`

Mutating `/api/*` endpoints accept either an authenticated admin session or the master-key bearer. `/internal/*` is gated by the control-key bearer unless `--dev-insecure` is enabled.

## Module map

```text
main.rs            boot, config, route registration
lib.rs             `AppState`, shared services, secret wrappers
api.rs             app CRUD, deploy, plan, usage reads
internal.rs        route/version/env feeds, usage ingest
auth_handlers.rs   login/signup/authorize/session flows
auth_service.rs    cookie JWTs, password auth
oauth.rs           Google OAuth config + flow helpers
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
  id uuid primary key,
  name text unique,
  plan_id text,
  deploy_hash text,
  api_key text,
  api_key_hash text,
  env_version bigint default 0,
  manifest_json text,
  created_at timestamptz,
  updated_at timestamptz
)
```

## Route and version feeds

`Registry::get_routes()` builds `RouteMap<Uuid, RouteEntry>` for the gateway. If `manifest_json` is missing, unparsable, or fails `Manifest::validate()`, the registry falls back to `Manifest::passthrough()` and logs a warning.

`Registry::get_versions()` builds `VersionMap<Uuid, AppVersionInfo>` for workers. The manifest in that feed is optional, so undeployed apps can still appear in the version map with `manifest = None`.

Gateway polls `/internal/routes` every 5 seconds. Worker polls `/internal/versions` every 5 seconds and fetches env snapshots lazily from `/internal/env/{app_id}` when `env_version` changes.

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

End-user auth still terminates in control:

```text
Gateway -> /auth/authorize?app_id=...&return=...
Control -> login / OAuth / consent
Control -> sets `__zs_session`
Gateway -> validates cookie and forwards `ZeroShip-User`
Worker/runtime -> reads the forwarded user context
```

## Notes

- `EnvStore` uses `zeroship_core::crypto` for encrypted-at-rest secrets, with primary + previous master-key support for rotation.
- `BundleStore` is still present on `AppState`, but deploy ingestion and runtime asset serving use `BlobStore`.
- Stripe support lives in `stripe_handlers.rs` and `stripe_store.rs`; worker metering still arrives through `/internal/usage`.
