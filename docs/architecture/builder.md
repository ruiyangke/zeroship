# Builder sandbox architecture

**Status: the sandbox/preview backend is not built by this repo.** It was
extracted to the standalone `zeroship-sandbox` project (sibling repository),
which owns the sandbox controller, the in-VM agent, and the Nomad +
Cloud-Hypervisor task driver. Nothing under `crates/` here builds or ships it,
and there is no `sandbox` service in `deploy/compose/docker-compose.yml`.

This page describes only the seam that remains on this side: how the platform
reaches that external service, and what this repo still configures.

## What lives where

| Concern | Where it lives |
| --- | --- |
| Sandbox lifecycle, file/exec APIs, preview HTTP + WebSocket proxy, snapshot/wake/cold-boot, pg-backed sandbox state, backend drivers (`docker`, `k8s`, `nomad-ch`) | the standalone `zeroship-sandbox` project |
| The console/builder app that calls it | extracted out of this monorepo too; control ingests it as a prebuilt `.zship` via `--bootstrap-console --console-zship` |
| Reaching the sandbox controller over HTTP (`SANDBOX_URL`, `SANDBOX_TOKEN`) | this repo |
| The Postgres schema and least-privilege roles the sandbox connects as (`sandbox_app` / `sandbox_audit` / `sandbox_gdpr`) | this repo, in `db/migrations-ts/` |

## The seam

The sandbox controller is a plain HTTP dependency reached by URL and bearer
token. Two settings carry it:

- `SANDBOX_URL` - the sandbox/preview backend base URL (non-secret config)
- `SANDBOX_TOKEN` - the controller bearer token (credential)

Control no longer seeds these onto the console's server-side env. The console is
not a built-in app: the install-time seed that upserted its app row, its OAuth
client and its runtime env (`OPENAI_API_KEY`, `SANDBOX_*`,
`ZEROSHIP_SDK_REGISTRY`) was removed, so the console is deployed like any other
creator app and its env is supplied the same way. The compose stack still sets
both on the `control` service in `deploy/compose/docker-compose.yml`, where the
commented-out `sandbox` service block records the same extraction.

## Shared database, separate deployment

The sandbox uses this deployment's shared Postgres rather than its own. The
schema and the least-privilege roles it connects as are defined here:

- `db/migrations-ts/20260702000100_schema_roles_extensions.ts` - roles and schema
- `db/migrations-ts/20260702000900_grants.ts` - the grants

The external controller connects **as** those roles; this repo never opens the
sandbox tables itself.

## Not on the request hot path

Sandbox work never touches end-user app serving. The gateway and worker have no
dependency on the sandbox controller, so an unavailable or absent sandbox
backend degrades the builder experience only - deployed creator apps keep
serving. See [Distributed architecture](../architecture/distributed.md).

## Historical detail

The design and operational notes written while the sandbox still lived here are
kept for history and are **not** a description of this tree:

- [Nomad + Cloud Hypervisor driver](../archive/nomad-driver-ch.md)
- [Sandbox pg-backed state](../archive/sandbox-pg-state.md)
- [Sandbox snapshot/restore](../archive/sandbox-snapshot-restore.md)
- [Sandbox preview URLs](../archive/sandbox-preview-urls.md)

## Related docs

- [Architecture overview](../architecture/overview.md) - the entry point and system map.
- [Distributed architecture](../architecture/distributed.md) - why the sandbox sits off the end-user request hot path.
