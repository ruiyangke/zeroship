# `.zship`

A `.zship` file is the deploy artifact handled by [crates/bundle/src/lib.rs](../../crates/bundle/src/lib.rs). The bundle crate treats it as the app package plus a JSON routing manifest.

## Manifest version

The current manifest schema version is `1`. Validation is defined in [crates/bundle/src/manifest.rs](../../crates/bundle/src/manifest.rs); unknown versions are rejected there.

## Current manifest shape

The top-level manifest struct in [crates/bundle/src/manifest.rs](../../crates/bundle/src/manifest.rs) includes:

- `version`
- `deploy_hash`
- `runtime_date`
- `worker`
- `resources`
- `schemas` (JSON schemas referenced by resource input/output metadata)
- `aliases`
- `transformer`
- `assets`
- `runtime_assets`
- `asset_version`
- `sourcemaps`
- `net`
- `metadata`
- `schedules`
- `workflows`
- `exports` (deprecated compatibility field; ignored)
- `migrations`
- `runtime_descriptor`

`worker`, when present, is:

```json
{
  "entry": "path/to/module",
  "modules": {
    "path/to/module": "sha256:..."
  }
}
```

`resources` is the routing source of truth. `migrations` carries committed
op.* migration artifacts, and `runtime_descriptor` points to the generated
`schema.runtime.json` blob folded from those migrations. `assets` holds
build-time static assets; `runtime_assets` holds runtime-emitted assets;
`asset_version` is the change counter the gateway uses to know when to resync
runtime assets.

`runtime_date` is copied from the resolved project config at build time and
retained as inert deployment metadata. The runtime does not branch on it or
select compatibility semantics from it.

`workflows` carries the build-discovered workflow declarations. Control and
worker-side workflow apply use it to reject starts or continue-as-new successors
for workflows that are not declared by the active deploy. `schedules` carries
build-discovered workflow schedule registrations.

## Network Requests

`net.requests` is a non-authoritative review hint for creator outbound egress:

```json
{
  "net": {
    "requests": [
      {
        "host": "api.example.com",
        "port": 443,
        "reason": "Call the upstream API"
      }
    ]
  }
}
```

The manifest never grants network access. Control compares these requests
against operator-authored rows in `zeroship.app_net_grants`; ungranted
host/port pairs remain denied until a grant row exists.

## Deprecated field

`exports` is still part of the wire struct, but the runtime no longer uses it
for schema discovery. New code relies on committed migrations plus the generated
`runtime_descriptor` blob (`schema.runtime.json`) instead. See
`sdks/bootstrap/src/runtime-entry.ts` and `crates/bundle/src/manifest.rs`.

## RPC resources

RPC procedures appear in `resources` with keys of the form `rpc:<wireId>`.
The Vite plugin derives those entries from `"use server"` modules and
`@zeroship/rpc/server` wrappers. Production builds reject procedures that rely
on export-name IDs; every deployed RPC must pin an explicit wrapper `id`.

The request path is not stored as a literal route per procedure. Gateway and
runtime agree on the reserved dispatch prefix:

```text
/__zeroship/v1/<wireId>
```

The manifest's `transformer` currently defaults to `"json"`. If an app opts
into `"superjson"`, the client and server must both use that transformer and
the consuming project must install `superjson`.

## Inspecting an artifact

There is no `zeroship inspect` command. It was removed in the artifact-layout
redesign and has not been replaced. The artifact is **not** opaque, though: a
`.zship` is a zstd-compressed tar, so standard tools read it.

```bash
# The whole manifest.
tar --zstd -xOf dist/app.zship manifest.json

# Every RPC the build actually declared, which is the usual question -
# a procedure missing here will 404 at runtime no matter what the source says.
tar --zstd -xOf dist/app.zship manifest.json | grep -oE '"rpc:[^"]+"'

# Archive layout: the manifest, then content-addressed blobs.
tar --zstd -tf dist/app.zship
```

Verified 2026-08-10 against `examples/bench/dist/app.zship`: the first command
prints the manifest, the second enumerates 14 `rpc:` entries, and the listing
shows `manifest.json` alongside `blobs/<sha256>` entries.

Piping the manifest through `jq` works too, but plain `grep` is deliberate here:
the manifest is emitted as one line, so `jq` is a convenience rather than a
requirement, and the commands above hold on a machine that has neither `jq` nor
the zeroship CLI on `PATH`.
