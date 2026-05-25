# `.zship`

A `.zship` file is the deploy artifact handled by [crates/bundle/src/lib.rs](crates/bundle/src/lib.rs). The bundle crate treats it as the app package plus a JSON routing manifest.

## Manifest version

The current manifest schema version is `1`. Validation is defined in [crates/bundle/src/manifest.rs](crates/bundle/src/manifest.rs); unknown versions are rejected there.

## Current manifest shape

The top-level manifest struct in [crates/bundle/src/manifest.rs](crates/bundle/src/manifest.rs) includes:

- `version`
- `deploy_hash`
- `worker`
- `resources`
- `schemas`
- `aliases`
- `transformer`
- `assets`
- `runtime_assets`
- `asset_version`
- `sourcemaps`
- `metadata`
- `exports` (deprecated compatibility field)

`worker`, when present, is:

```json
{
  "entry": "path/to/module",
  "modules": {
    "path/to/module": "sha256:..."
  }
}
```

`resources` is the routing source of truth. `assets` holds build-time static assets; `runtime_assets` holds runtime-emitted assets; `asset_version` is the change counter the gateway uses to know when to resync runtime assets.

## Deprecated field

`exports` is still part of the wire struct so older archives continue to deserialize, but the runtime no longer uses it for schema discovery. New code should rely on `default.schema` instead. See [sdks/bootstrap/src/runtime-entry.ts](sdks/bootstrap/src/runtime-entry.ts) and [crates/bundle/src/manifest.rs](crates/bundle/src/manifest.rs).
