# Builder sandbox architecture

This worktree's builder-side infrastructure is the sandbox service in `crates/sandbox`. It owns live dev sandboxes, preview proxying, and the snapshot/restore control flow used by the builder stack.

## Current scope

The current implementation is platform-side only:

- sandbox lifecycle and file/exec APIs
- preview HTTP and WebSocket forwarding
- pg-backed sandbox metadata
- sealed-record persistence
- snapshot, wake, and cold-boot admin flows

This file does not describe any separate `zeroship/editor` repository or a Docker-only preview architecture; the shipping code is the sandbox controller in this repo.

## Relevant files

- [crates/sandbox/src/main.rs](crates/sandbox/src/main.rs): HTTP surface and service boot
- [crates/sandbox/src/handlers.rs](crates/sandbox/src/handlers.rs): creator-facing sandbox API
- [crates/sandbox/src/backend/mod.rs](crates/sandbox/src/backend/mod.rs): backend enum
- [crates/sandbox/src/backend/nomad_ch.rs](crates/sandbox/src/backend/nomad_ch.rs): Nomad + Cloud Hypervisor backend
- [crates/sandbox/src/preview.rs](crates/sandbox/src/preview.rs): HTTP preview proxy
- [crates/sandbox/src/preview_ws.rs](crates/sandbox/src/preview_ws.rs): WebSocket preview forwarder
- [crates/sandbox/src/admin_handlers.rs](crates/sandbox/src/admin_handlers.rs): admin, snapshot, wake, and cold-boot routes

## Backends

`Backend` currently supports three runtime shapes:

| Backend | Role today |
| --- | --- |
| `docker` | local/dev sandbox runtime |
| `k8s` | pod-backed sandbox runtime |
| `nomad-ch` | Nomad job per sandbox using the Go `ch` task driver and `cloud-hypervisor` |

The recent sandbox snapshot/restore work is wired against the `nomad-ch` path. The current bare-metal VM backend is the Go-plugin Nomad task driver described in `nomad_ch.rs`, not the older wrapper-driven model.

## HTTP surface

Creator-facing routes in `main.rs`:

- `POST /sandboxes`
- `GET /sandboxes`
- `GET /sandboxes/{id}`
- `DELETE /sandboxes/{id}`
- `POST /sandboxes/{id}/exec`
- `GET /sandboxes/{id}/file-tree`
- `GET|PUT|DELETE /sandboxes/{id}/files/{path}*`

Preview routes:

- HTTP proxy: `ANY /sandboxes/{id}/preview/{port}/{path}*`
- WebSocket proxy: separate listener from `preview_ws.rs`
- share-token routes under `/sandboxes/{id}/preview/{port}/share`

Operator/admin routes include sandbox listing, per-user export/delete, host listing, snapshot, wake, wake polling, and cold boot.

## Preview path

Preview is controller-driven:

```text
client
  -> sandbox controller preview route
  -> controller authorizes the request
  -> controller forwards to the sandbox agent/runtime
```

The controller always supports path-shaped preview URLs. Current preview helpers also synthesize hostnames of the form `preview-<slug>-<port>.preview.zeroship.dev` for forwarded headers and share links.

## Persistence and restore

The sandbox service now owns:

- in-memory registry state
- optional PostgreSQL-backed sandbox state
- sealed-record persistence
- snapshot storage wiring
- startup restore and wake flows

Snapshot/wake behavior is feature-gated by sandbox config. The admin handlers and `nomad-ch` backend are the places to read first when changing that flow.

## Where to start

| Working on | Start here |
| --- | --- |
| sandbox API behavior | [handlers.rs](crates/sandbox/src/handlers.rs) |
| backend selection | [backend/mod.rs](crates/sandbox/src/backend/mod.rs) |
| Nomad + CH runtime | [nomad_ch.rs](crates/sandbox/src/backend/nomad_ch.rs) |
| preview proxy | [preview.rs](crates/sandbox/src/preview.rs), [preview_ws.rs](crates/sandbox/src/preview_ws.rs) |
| snapshot/restore | [admin_handlers.rs](crates/sandbox/src/admin_handlers.rs), `docs/runbooks/sandbox-nomad-ch.md` |
