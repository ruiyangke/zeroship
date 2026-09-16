# Architecture overview

zeroship is two systems that share storage, auth, and PostgreSQL. This page is the short map; the crate-specific docs cover the details.

## System map

```text
Creator platform
  Creator UI / CLI
    -> control plane (`crates/control`)
    -> PostgreSQL
    -> bundle/blob root (`crates/bundle`, current impl: `LocalDiskBlobStore`)

App runtime
  End users
    -> gateway (`crates/gateway`)
    -> worker (`crates/worker`)
    -> runtime (`crates/runtime`)
    -> env.{db,kv,storage} plugins

Builder sandbox (NOT in this repo)
  Editor / operator traffic
    -> sandbox controller (standalone `zeroship-sandbox` project)
    -> backend runtime (docker, k8s, or nomad-ch)
```

## Crates

| Component | Crate | Current responsibility |
| --- | --- | --- |
| Control plane | `crates/control` | App CRUD, deploy ingest, auth routes, env/secrets, Stripe state, route/version registry |
| Gateway | `crates/gateway` | App lookup, compiled-manifest dispatch, JWT/cookie auth gate, static asset serving, worker proxying |
| Worker | `crates/worker` | Per-thread V8 runtime cache, bundle/env sync, request execution, usage reporting |
| Runtime | `crates/runtime` | V8 embedder, Web APIs, RPC/HTTP bridge, async pump, native plugin host |
| Bundle | `crates/bundle` | `.zship` manifest types, blob store trait, ingest path, legacy bundle store types |
| Core | `crates/core` | Shared wire types, auth helpers, typed IDs, observability helpers |
| DB ORM | `crates/zeroship-data-orm` | Backend-independent Rust API, SQL compilation, transactions, and database adapters |
| DB V8 adapter | `crates/zeroship-data-v8` | `env.db.*` |
| KV storage | `crates/zeroship-kv` | Backend contract and Redis/redb implementations |
| KV V8 binding | `crates/zeroship-kv-v8` | `env.kv.*` |
| Object storage | `crates/zeroship-storage` | Scoped Rust operations, LocalFs and S3 backends |
| Storage V8 binding | `crates/zeroship-storage-v8` | `env.storage.*` |

The sandbox/preview backend used to be a crate here. It is not: it was extracted
to the standalone `zeroship-sandbox` project and is not built by this repo. The
control plane reaches it over HTTP (`SANDBOX_URL`/`SANDBOX_TOKEN`), and it shares
this deployment's Postgres through the `sandbox_*` roles.

## End-user request path

```text
1. Gateway resolves app name from path or Host.
2. Gateway looks up `RouteEntry` in `RouteCache`.
3. Gateway compiles/uses `Manifest.resources`:
   - static asset -> `router/static_serve.rs`
   - worker RPC/SSR -> proxy to worker
   - redirect/rewrite -> handled in gateway
4. Worker resolves or loads the app's V8 runtime.
5. `Runtime::call_fetch_handler(...)` runs user code and returns `FetchOutcome`.
```

Auth today is cookie/JWT-based at the gateway. When a session is valid, the gateway forwards an HMAC-signed `ZeroShip-User` header to the worker, as defined in the auth flow contract ([auth.md](../reference/auth.md)).

## Deploy path

```text
1. CLI uploads `application/x-zship` to `POST /api/apps/{id}/deploy`, naming
   the deploy command in its `Idempotency-Key`.
2. Control streams the body to temp storage, then calls `zeroship_bundle::ingest`.
3. Ingest validates `manifest.json`, streams `blobs/<hash>` into `BlobStore`,
   and returns `deploy_hash` + canonical `manifest_json`.
4. Control commits the `apps` row, the deployment, the command receipt and a
   lifecycle intent in one transaction; its publisher later delivers the
   intent to the workflow manager.
5. Gateway picks up the new route state from `/internal/routes`.
6. Worker picks up the new version state from `/internal/versions` and fetches
   the worker-entry blob from `BlobStore`.
```

The `.zship` deploy archive and artifact layout are specified in [zship.md](../reference/zship.md).

For SDK-facing contracts:
- Database plugin behavior and `env.db` assumptions are in [db.md](../reference/db.md).
- KV plugin contract (TTL, counters, list semantics) is in [kv.md](../reference/kv.md).

## Read next

- [Distributed architecture flow](../architecture/distributed.md): end-to-end request sequence for creator-facing and end-user paths.
- [Gateway routing](../architecture/gateway-routing.md): manifest matching, dispatch, and request path control.
- [Control plane architecture](../architecture/control-plane.md): app deploy lifecycle, route registry, and metadata updates.
- [Runtime architecture](../architecture/runtime.md): V8 execution, async model, native host bridges, and request execution.
- [Blob store and bundle storage](../architecture/blob-store.md): `BlobStore`, `.zship` blobs, and object layout.
- [Builder sandbox](../architecture/builder.md): where the sandbox/preview backend went, and the seam this repo still owns.

## Current architecture notes

- The runtime stack is all compio/io_uring. There is no tokio in the app-serving path.
- The gateway's hot path is the compiled resource tree from `Manifest.resources`, not the older rule walker.
- The shipping blob-store backend in this worktree is `LocalDiskBlobStore`; gateway-side memory and disk LRUs live in `crates/zeroship-gateway/src/blob_cache.rs`.
- Sandbox infrastructure is a separate service. It is not on the end-user request path.
