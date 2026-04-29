# `.zsapp` — deploy artifact format

**Schema:** `version: 2`

A single artifact emitted by the build pipeline and ingested by the control plane. Carries the worker code, all client-side assets (HTML, JS, CSS, images, prerendered pages), source maps, and a manifest naming everything by content hash. Worker code is represented as a uniform `{ entry, modules }` map — single-module today, code-split later, same shape either way.

## Container

`tar.zst` (zstd-compressed tar). MIME: `application/x-zsapp`.

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

JSON, schema version `2`. Fields and their semantics:

```jsonc
{
  "version": 2,                                          // schema version (u16); readers reject != 2
  "deploy_hash": "<sha256>",                             // computed; see "deploy_hash"
  "worker": <WorkerCode> | null,                         // see "Worker code" below
  "rules": [<Rule>, ...],                                // routing rules
  "assets": {                                            // path -> asset metadata; ALL static bytes
    "/index.html": {                                     //   the SPA shell, prerendered HTML,
      "hash": "<sha256>",                                //   JS chunks, CSS, images, public files —
      "content_type": "text/html",                       //   every static file is just an asset
      "size": 2048,
      "cache": null                                      // optional CacheCtl override
    }
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

### Worker code

`worker` is the JS that runs in a V8 worker isolate (the user's `default.fetch` handler plus its module graph). The shape is uniform — always an `entry` specifier plus a `modules` map of specifier → blob hash. Single-bundled servers have one entry in `modules`; code-split servers have many. There is no enum or discriminator.

```jsonc
// Typical: vite/esbuild emits one bundled file
"worker": {
  "entry":   "index.js",
  "modules": { "index.js": "<sha256>" }
}

// Future: code-splitting via V8's module-resolve callback
"worker": {
  "entry":   "src/index.js",
  "modules": {
    "src/index.js":      "<sha256>",
    "src/routes/api.js": "<sha256>",
    "src/lib/db.js":     "<sha256>"
  }
}

// SSG-only — no JS runs in V8
"worker": null
```

Rust type:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerCode {
    pub entry:   String,                       // specifier V8 evaluates first; must be a key in `modules`
    pub modules: HashMap<String, String>,      // specifier → blob hash
}
```

The worker, on cold start:
1. Fetches `manifest.worker.modules[entry]` blob.
2. Constructs `ModuleEntry { specifier: entry, source: bytes_as_utf8 }`.
3. Hands it to V8 with a module-resolve callback that looks up further imports against the `modules` map and fetches their blobs lazily through `BlobStore`.

For the single-module case, the callback is never invoked — V8 evaluates `index.js` and returns. For the multi-module case, V8 walks the import graph; each `import` triggers a callback → blob fetch → module construction.

The build pipeline emits a single-module shape today. Multi-module is unlocked when there's a concrete code-splitting use case; the schema is already forward-prepared.

### No frontend entry

Symmetric question to "what's the worker entry?" — answer: there isn't one, by design.

The platform never executes frontend code. Browsers do. The platform's job for the frontend is **to ship bytes**: HTML files, JS chunks, CSS, images. All of those are entries in `assets[]`, treated identically.

What the browser loads first is decided by the rules (which HTML gets served for a given URL) and by the HTML's own `<script>` / `<link>` tags. Each rendering mode handles "what loads first" without a manifest field:

| Mode | What the browser loads first | How |
| --- | --- | --- |
| CSR (SPA) | `/index.html` (or whatever the SPA-fallback rule names) | A `Match::Any → Static{ try: ["$path", "/index.html"] }` rule serves it for unmatched routes. The shell's `<script>` tag points to the JS entry chunk. |
| SSG | The prerendered HTML for that route | A per-route `Match::Exact → Static{ try: ["/about.html"] }` rule serves it. Each prerendered HTML embeds its own `<script>` tags at build time. |
| SSR | Worker-generated HTML | The worker's `default.fetch` returns the HTML. The server bundle's source code references hashed JS filenames via Vite's build-time manifest (separate from this deploy manifest). |

For Vite-using apps, the build pipeline's `vite build --ssr` step imports Vite's emitted `manifest.json` (Vite's own asset manifest, NOT this deploy manifest) so the server bundle knows which hashed filename to inject. That manifest is a build-time artifact baked into the server bundle's source — it never appears in the deploy manifest.

So: `worker.entry` exists because the platform actually evaluates that module in V8. There's no frontend-side equivalent because the platform never evaluates frontend code; it just serves bytes.

### Required vs optional

| Field | Required | Notes |
| --- | --- | --- |
| `version` | yes | MUST be `2`; readers reject any other value |
| `deploy_hash` | computed | absent or empty in build output; control plane fills it on receipt |
| `worker` | iff worker rules exist | a `WorkerCode { entry, modules }` or null. null/missing means SSG-only |
| `rules` | yes | may be `[]` for asset-only static deploys (gateway 404s on no match) |
| `assets` | yes | may be `{}` |
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

- Every hash appearing in `assets[].hash`, `sourcemaps` keys/values, and `worker.modules` values MUST correspond to a tar entry in the archive. The client always uploads every referenced blob — no "we already have it" claims (server-internal dedup is invisible to the client).
- Every asset path referenced by a rule's `try` chain (e.g., `try: ["/about.html"]`) MUST exist as a key in `assets` (or be substituted by `$path`/captures into a path that does). Validation responsibility: build pipeline emits sane manifests; control plane's `Manifest::validate()` rejects dangling references.

## Multi-tenancy and possession proof

Blobs share a single global keyspace (`blobs/<hash>`). Cross-tenant dedup is **safe by construction** because two constraints together provide a cryptographic possession proof:

1. **Clients always upload every blob in the deploy.** No client-side "skip if hash already exists" optimization. The protocol has no `has_blob` query.
2. **The server verifies `sha256(bytes) == hash`** for every uploaded blob. Mismatches are rejected.

Together: any successful deploy referencing hash `X` proves the deployer had the bytes for `X`. Finding bytes that hash to a value without possessing them is preimage-hard (2^256 work for SHA-256). When App A and App B both reference the same hash, they each independently demonstrated possession — there is no privilege to escalate via deduplication.

Consequences:

- **No public/private flag** on assets. Single keyspace.
- **No per-tenant namespacing** in storage. `blobs/<hash>` is global.
- **`has_blob` is a server-internal trait method** used by ingestion to discard duplicate writes. It is never exposed over HTTP — exposing it would create an oracle that defeats the possession-proof argument.
- **The gateway serves blobs only via manifest-routed paths** (`GET /index.html` resolves through `assets["/index.html"].hash`). It never exposes `/blobs/<hash>` directly. Possession is preserved end-to-end: anyone who can fetch hash `X` already had the bytes via their manifest.

Operational concerns that aren't security:

- **Billing**: each tenant is billed for `sum(assets[].size)` over the blobs they reference, regardless of whether storage actually allocated bytes for them (dedup is the platform's optimization, not the customer's discount).
- **GC**: manifest-driven mark-and-sweep walks `manifests/*/*.json`, collects the union of referenced hashes, deletes unreferenced blobs older than the retention window.
- **Compliance opt-out**: a per-tenant `dedicated_storage: bool` flag (not currently supported) would disable dedup for tenants whose contracts forbid shared infrastructure.

## Control plane ingestion

```
POST /api/apps/{id}/deploy
Authorization: Bearer <master-key>
Content-Type: application/x-zsapp
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
   - Every hash referenced in `assets`, `sourcemaps`, and `worker.modules` MUST appear as a tar entry later in the stream. (No "we already have it" claims — clients always include every referenced blob.)
   - `Manifest::validate()` rule shadowing checks pass.
4. **Compute `deploy_hash`** from the canonical (deploy_hash-omitted) manifest.
5. **For each subsequent tar entry** `blobs/<hash>`:
   - Verify `sha256(entry_bytes) == hash`. Reject on mismatch — this is the possession-proof checkpoint.
   - If `blob_store.has_blob(hash)`: discard the bytes (dedup hit, internal optimization invisible to the client).
   - Else: write via `blob_store.put_blob(hash, bytes)`.
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

`version: u16`. The schema version is `2`. Readers MUST reject any `version` value they don't explicitly support; never trust an unknown manifest as an unparsed JSON blob.

Adding fields stays backward-compatible (readers ignore unknowns within the same major version). Changing semantics or removing fields requires a `version` bump.

## What this format intentionally does NOT carry

- App name, plan, env vars, secrets — those live in the database.
- Stripe / billing config.
- Auth provider credentials.
- API keys.

Deploys are about code + assets + routing. Identity and configuration are a separate plane.
