# `.zsdeploy` — deploy artifact format

**Version:** 1
**Status:** Draft
**Replaces:** `.appbundle`-only uploads + per-file `PUT /api/apps/{id}/assets/{path}`.

A single artifact emitted by the build pipeline and ingested by the control plane. Carries the server bundle, all client assets, prerendered HTML, source maps, and a manifest naming everything by content hash. Hard-cut replacement: the old paths go away in the same milestone the new ones land.

## Container

`tar.zst` (zstd-compressed tar). MIME: `application/x-zsdeploy`.

```
manifest.json                 ← MUST be the first tar entry
blobs/<hash>                  ← N entries, one per content-addressed blob
```

Where `<hash>` is the lowercase 64-char hex SHA-256 of the blob's raw bytes (uncompressed, unwrapped).

Blobs are stored in the tar **without further compression** — zstd at the archive level handles compression. This keeps individual entries hashable as-is.

## Hash semantics

Every blob's hash is `sha256(raw bytes)`. This matches:

- The filename inside the tar.
- The storage key under `blobs/<hash>`.
- The `hash` field in any manifest reference to the blob.
- The HTTP `ETag` the gateway emits when serving the blob.

A blob is one file. **One file ↔ one blob.** No grouping, no chunk-level dedup. See `docs/architecture/blob-store.md` for the rationale.

## `manifest.json`

JSON, schema version `1`. Fields and their semantics:

```jsonc
{
  "version": 1,                                          // schema version (u16)
  "deploy_hash": "<sha256>",                             // computed; see "deploy_hash"
  "server_bundle": "<sha256>",                           // hash of the server JS blob; null for SSG-only
  "rules": [<Rule>, ...],                                // routing rules — same shape as today's Manifest.rules
  "assets": {                                            // path -> asset metadata
    "/index.html": {
      "hash": "<sha256>",
      "content_type": "text/html",
      "size": 2048,
      "cache": null                                      // optional CacheCtl override
    }
  },
  "prerendered": {                                       // route -> asset path (must exist in `assets`)
    "/about": "/_prerendered/about.html"
  },
  "runtime_assets": {},                                  // MUST be {} on fresh deploy; mutated post-deploy
  "asset_version": 0,                                    // MUST be 0 on fresh deploy
  "sourcemaps": {                                        // asset hash -> sourcemap blob hash
    "<asset hash>": "<sourcemap hash>"
  },
  "metadata": {                                          // informational; not load-bearing
    "compiler": "@zeroship/vite-plugin@0.x",             // optional
    "built_at": "2026-04-29T12:34:56Z"                   // RFC 3339, REQUIRED
  }
}
```

### Required vs optional

| Field | Required | Notes |
| --- | --- | --- |
| `version` | yes | reject unknown values |
| `deploy_hash` | computed | absent or empty in build output; control plane fills it on receipt |
| `server_bundle` | iff worker rules exist | null/missing means SSG-only |
| `rules` | yes | may be `[]` for asset-only static deploys (gateway 404s on no match) |
| `assets` | yes | may be `{}` |
| `prerendered` | yes | may be `{}` |
| `runtime_assets` | yes | MUST be `{}` |
| `asset_version` | yes | MUST be `0` |
| `sourcemaps` | yes | may be `{}` |
| `metadata.built_at` | yes | RFC 3339 string |

### `deploy_hash` computation

1. Take the manifest with `deploy_hash` field **omitted entirely** (not just empty).
2. Canonicalize: sort all object keys lexicographically, no insignificant whitespace, UTF-8.
3. Compute `sha256` of the canonical bytes.
4. The result IS `deploy_hash`. Insert into the manifest before storing.

Verification: omit `deploy_hash`, recanonicalize, rehash, compare. Cheap.

This means **any change to any blob's content cascades to a new `deploy_hash`** because the manifest references blobs by hash. Reproducible builds with deterministic compilers produce reproducible deploy hashes.

### Cross-references

- Every hash appearing in `assets[].hash`, `prerendered[]` (the resolved asset path's `assets[].hash`), `sourcemaps` keys/values, and `server_bundle` MUST correspond to a blob in the archive — UNLESS it already exists in the control plane's blob store (dedup).
- Every key in `prerendered` MUST be a path that won't conflict with `rules` matches. Validation responsibility: build pipeline emits sane manifests; control plane's `Manifest::validate()` rejects ambiguous ones.

## Control plane ingestion

```
POST /api/apps/{id}/deploy
Authorization: Bearer <master-key>
Content-Type: application/x-zsdeploy
Content-Length: <bytes> | Transfer-Encoding: chunked
Body: streaming .tar.zst
```

Server algorithm:

1. **Stream-decompress** the body through zstd. Bound the decompressed size (default cap: 256 MB; configurable). Reject deploys exceeding the cap.
2. **Stream-untar.** First entry MUST be `manifest.json`. Reject otherwise.
3. **Parse + validate** the manifest:
   - `version` is supported.
   - All required fields present.
   - `runtime_assets == {}`, `asset_version == 0`.
   - All cross-references resolve (every hash either has a tar entry or a known existing blob).
   - `Manifest::validate()` rule shadowing checks pass.
4. **Compute `deploy_hash`** from the canonical (deploy_hash-omitted) manifest.
5. **For each subsequent tar entry** `blobs/<hash>`:
   - Verify `sha256(entry_bytes) == hash`. Reject on mismatch.
   - If `blob_store.has_blob(hash)`: skip (dedup hit).
   - Else: stream the entry to `blob_store.put_blob(hash, ...)`.
6. **Write the manifest** under `manifests/<app_id>/<deploy_hash>.json` (with `deploy_hash` inserted).
7. **Atomic update**:
   ```sql
   UPDATE apps
   SET deploy_hash = $1, manifest_json = $2, updated_at = NOW()
   WHERE id = $3
   ```
   Where `manifest_json` is the canonical JSON (with `deploy_hash` inserted) — kept inline for fast load on `list_routes()`.

### Failure modes

| Cause | Status | Body |
| --- | --- | --- |
| Tar entry hash mismatch | 400 | `{"error": "blob hash mismatch", "expected": "...", "got": "..."}` |
| `manifest.json` not first entry | 400 | `{"error": "manifest must be first tar entry"}` |
| Manifest schema violation | 400 | `{"error": "invalid manifest", "detail": "..."}` |
| Unknown `version` | 400 | `{"error": "unsupported manifest version"}` |
| Decompressed size > cap | 413 | `{"error": "deploy too large", "cap_bytes": ...}` |
| Master key invalid | 401 | (existing) |
| Blob store unavailable | 503 | `{"error": "blob store unavailable"}` |

All failures are atomic — no partial deploy state lingers. The `apps` row update is the commit point; until then, blobs may be uploaded but the deploy is not active.

## Storage layout

Object storage (or `LocalDiskBlobStore`):

```
<root>/blobs/ab/cd1234...                 ← content-addressed; first 2 hex chars used as a shard prefix
<root>/manifests/<app_id>/<deploy_hash>.json
```

The shard prefix prevents directory-explosion on millions of blobs. Implementations are free to use a flat layout (`<root>/blobs/<hash>`) for small deployments; readers MUST handle both.

## Rollback

```sql
UPDATE apps SET deploy_hash = $previous_hash WHERE id = $app_id
```

The previous deploy's manifest is still at `manifests/<app_id>/<previous_hash>.json`; the blobs it referenced are still in `blobs/<hash>` (untouched by GC because the manifest is on disk). One UPDATE = full revert.

## Garbage collection

Blobs may outlive the apps that uploaded them (cross-app dedup, deploy-history retention). Strategy:

- **Background mark-and-sweep**, weekly.
- Walk all manifests under `manifests/*/*.json`. Collect the union of referenced hashes.
- Delete `blobs/<hash>` files older than `retention_days` (default 30) that aren't in the referenced set.

Refcounting is rejected as too write-amplifying for the busy-store case; eventual consistency is fine here.

## Limits

| Limit | Default | Configurable |
| --- | --- | --- |
| Decompressed deploy size | 256 MB | yes |
| Single blob size | 16 MB | yes |
| Number of blobs per deploy | 10,000 | yes |
| Manifest size | 1 MB | yes |

Validate at the streaming boundary, not after the fact — reject early.

## Versioning

`version: u16`. Adding fields is backward-compatible (readers ignore unknowns). Changing semantics or removing fields requires a `version` bump. Control plane MUST reject unknown versions; never trust an unknown manifest as an unparsed JSON blob.

## What this format intentionally does NOT carry

- App name, plan, env vars, secrets — those live in the database.
- Stripe / billing config.
- Auth provider credentials.
- API keys.

Deploys are about code + assets + routing. Identity and configuration are a separate plane.
