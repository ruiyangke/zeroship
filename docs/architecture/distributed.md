# Distributed architecture

Current zeroship deployment is multi-process and single-region. The platform is split into separate binaries that communicate over HTTP and a shared blob/bundle root.

## Topology

```text
creator / CLI
  -> control plane (`crates/control`)
     -> PostgreSQL
     -> bundle/blob root

end user
  -> gateway (`crates/gateway`)
     -> route cache from control
     -> static bytes from blob root
     -> CHWBL proxy to worker

worker (`crates/worker`)
  -> version map from control
  -> env snapshots from control
  -> worker-entry blobs from blob root
  -> V8 runtime cache

builder / operator
  -> sandbox controller (`crates/sandbox`)
```

## Current coordination model

The current system is pull-based:

- gateway polls `/internal/routes` every 5 seconds
- worker polls `/internal/versions` every 5 seconds
- worker fetches `/internal/env/{app_id}` when `env_version` changes

There is no event bus or push fanout in the current code path. Polling keeps the
data flow one-directional (control is never called synchronously on the request
hot path), at the cost of up-to-5-second propagation lag after a deploy.

CHWBL (Consistent Hashing with Bounded Loads) is how the gateway picks which
worker handles a request: requests for the same app hash to the same worker so
its V8 isolate stays warm, while the bounded-load cap spills to another worker
when a hot app would otherwise overload one node.

## End-user request sequence

```text
1. Gateway resolves app name and loads the compiled route.
2. Gateway dispatches from `Manifest.resources`.
3. Static response:
   - serve from gateway cache/blob store
4. Worker response:
   - pick worker via CHWBL
   - forward request to worker
   - worker runs the app's cached V8 runtime or loads it from blob store
```

## Deploy sequence

```text
1. CLI uploads `.zship` to control.
2. Control ingests blobs + manifest and updates the `apps` row.
3. Gateway sees the new route state on its next `/internal/routes` poll.
4. Worker sees the new version state on its next `/internal/versions` poll.
5. Worker fetches the new worker-entry blob and swaps the cached runtime.
```

This is request-boundary eventual propagation. There is no cross-service transactional switchover.

## Builder sandbox path

The builder stack is outside the app-serving hot path:

- `crates/sandbox` exposes sandbox lifecycle, preview, and admin endpoints
- the current bare-metal VM backend is `nomad-ch` in [nomad_ch.rs](../../crates/sandbox/src/backend/nomad_ch.rs)
- operator details live in `docs/runbooks/sandbox-nomad-ch.md`

Snapshot, restore, wake, and cold-boot flows are part of the sandbox service, not the gateway/worker request path.

## Current boundaries

- Single region
- HTTP polling between control and gateway/worker
- Local-disk blob store implementation
- No custom edge POP layer in this repo
- No multi-region route propagation or data replication in the shipping code path

## Related docs

- [Architecture overview](../architecture/overview.md) — the entry point; start here for the system map.
- [Gateway routing](../architecture/gateway-routing.md) — manifest dispatch and the request hot path.
- [Control plane](../architecture/control-plane.md) — where the `/internal/*` route and version feeds come from.
- [V8 runtime](../architecture/runtime.md) — what runs inside the worker once a request arrives.
- [Blob store](../architecture/blob-store.md) — the shared content-addressed bundle/asset root.
- [Docker Compose runbook](../runbooks/docker-compose.md) — bring the multi-process stack up locally.
