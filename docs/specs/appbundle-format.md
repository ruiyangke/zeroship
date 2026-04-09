# .appbundle Format Specification

**Version:** 1
**Date:** 2026-04-09
**Status:** Draft

## Overview

`.appbundle` is a binary format for packaging JavaScript modules for the appbase runtime. It stores multiple ES modules with per-module zstd compression, enabling lazy decompression — only imported modules are decompressed and compiled.

## Design goals

1. **Fast cold start** — small transfer from object storage, stream manifest before modules
2. **Low memory** — unused modules stay compressed in memory (~4x smaller)
3. **Integrity** — SHA-256 hash verifiable before parsing any modules
4. **No escaping** — module sources stored as raw bytes, not JSON-escaped strings
5. **No runtime compression dependency for transport** — HTTP gzip handles wire compression; zstd is only for in-memory storage
6. **Simple** — parseable in ~100 lines of code, no external format library

## Binary layout

```
Offset  Size    Field
──────  ──────  ─────────────────────────────────
0       4       Magic: ASCII "APPB"
4       4       Format version: u32 big-endian (currently 1)
8       32      SHA-256 hash of all bytes after this field
40      4       Manifest length: u32 big-endian
44      N       Manifest: UTF-8 JSON (uncompressed)
44+N    4       Module count: u32 big-endian
                For each module:
                  4       Specifier length: u32 big-endian
                  S       Specifier: UTF-8 bytes
                  1       Module type: u8
                  4       Compressed source length: u32 big-endian
                  C       Source: zstd-compressed UTF-8 bytes
                  4       Compressed source map length: u32 big-endian (0 if none)
                  M       Source map: zstd-compressed UTF-8 bytes (absent if length = 0)
```

All multi-byte integers are **unsigned, big-endian**.

## Sections

### Magic and version

The first 8 bytes identify the format. Readers must reject files where magic ≠ `APPB` or version > their supported version.

### Integrity hash

Bytes 8–39 contain a SHA-256 digest of **everything from byte 40 onward** (manifest + all modules). This allows integrity verification before parsing any module content.

Verification:
1. Read bytes 8–39 (expected hash)
2. SHA-256 hash bytes 40 to EOF
3. Compare — reject on mismatch

This catches corruption in object storage, truncated transfers, and tampering.

### Manifest

A UTF-8 JSON object containing metadata about the bundle. The manifest is **not compressed** — it must be readable without any decompression step.

Fields:

| Field | Type | Required | Description |
|-------|------|:--------:|-------------|
| `name` | string | Yes | Application name |
| `version` | integer | Yes | Deploy version (monotonically increasing) |
| `entry` | string | Yes | Entry module specifier (e.g., `"index.js"`) |
| `exports` | string[] | No | Known exported function names (informational, not authoritative) |
| `compiler` | string | No | Compiler identifier (e.g., `"appbase 0.1.0"`) |

The manifest must not exceed 64 KB.

The `exports` field is **informational only** — it reflects what the compiler detected at build time. The runtime must not rely on it for dispatch; JS is dynamic and exports may differ at evaluation time.

### Modules

Each module is stored as:
1. **Specifier** — the import path (e.g., `"index.js"`, `"utils/helper.js"`)
2. **Type** — what kind of module (determines how V8 loads it)
3. **Source** — zstd-compressed UTF-8 source code
4. **Source map** — zstd-compressed UTF-8 source map (optional)

Module types:

| Value | Type | Description |
|:-----:|------|-------------|
| 0 | `esModule` | ES module (import/export syntax) |
| 1 | `json` | JSON module (import produces an object) |
| 2 | `text` | Raw text (import produces a string) |
| 3 | `data` | Raw binary (import produces ArrayBuffer) — source is raw bytes, not UTF-8 |

The **first module listed** should be the entry module (matching `manifest.entry`), though readers should use the manifest's `entry` field for resolution, not positional ordering.

### Per-module compression

Each module's source is independently zstd-compressed. This enables:

- **Lazy decompression** — only decompress modules that are actually imported
- **Independent access** — decompress any module without touching others
- **Efficient memory** — unused modules occupy ~25% of their uncompressed size

Compression level is left to the writer. Recommended: zstd level 3 (fast compression, good ratio). Readers must support any valid zstd frame.

For modules smaller than 64 bytes, compression may increase size. Writers may store such modules uncompressed by using zstd level 0 (passthrough), which produces a valid zstd frame containing the raw data.

### Source maps

Source maps are optional per module. When present, they follow the same zstd compression as sources. When absent, the compressed source map length is 0 and no bytes follow.

Source maps follow the standard Source Map v3 format (JSON with `version`, `sources`, `mappings` fields).

## Storage architecture

```
Object storage (R2/S3):        Database:
  bundles/                       apps table:
    {app_id}/                      id
      v3.appbundle                 version
      v2.appbundle                 content_hash (SHA-256)
                                   bundle_url
                                   bundle_size
                                   entry
                                   name
```

The database stores metadata and a URL to the bundle in object storage. The bundle itself is never stored in the database.

Previous bundle versions are retained in object storage for rollback. The database's `version` field indicates the active version.

## Loading sequence

1. **Fetch** — download `.appbundle` from object storage URL in database
2. **Verify** — check magic, version, SHA-256 hash
3. **Parse manifest** — read entry point, module count
4. **Index modules** — read specifiers and byte offsets for each module (do not decompress)
5. **Decompress entry module** — zstd decompress the entry module's source
6. **Compile entry module** — V8 `compile_module`
7. **Discover imports** — `get_module_requests()` on compiled entry module
8. **Decompress + compile imported modules** — only the ones transitively imported
9. **Instantiate + evaluate** — V8 walks the module graph, resolve callback returns pre-compiled modules
10. **Serve requests** — unused modules remain compressed in memory

Steps 5–8 embody lazy decompression + lazy compilation. A bundle with 1000 modules where only 5 are imported: 5 decompressed, 5 compiled, 995 stay as compressed bytes.

## Size estimates

| App type | Modules | Uncompressed | Compressed (.appbundle) | In-memory (lazy) |
|----------|--------:|-------------:|------------------------:|-----------------:|
| Simple API | 1 | 5 KB | 2 KB | 5 KB |
| Medium app | 10 | 50 KB | 15 KB | ~20 KB |
| Large app (bundled) | 1 | 500 KB | 100 KB | 500 KB |
| Large app (unbundled) | 200 | 2 MB | 500 KB | ~100 KB (if 10 imported) |

## CLI integration

```
# Build and package
appbase build ./src --output app.appbundle

# Inspect without extracting
appbase inspect app.appbundle
# Output:
#   name: my-app
#   version: 3
#   entry: index.js
#   modules: 42 (380 KB uncompressed, 95 KB compressed)
#   hash: sha256:a1b2c3...

# Deploy
appbase deploy app.appbundle --app my-app
# Uploads to object storage, updates database metadata
```

## Comparison with alternatives

| | .appbundle | ZIP | eszip | JSON blob |
|---|---|---|---|---|
| Per-module compression | Yes (zstd) | Yes (deflate) | No | No |
| Lazy decompression | Yes | No (must extract) | Yes (streaming) | No |
| Integrity hash | SHA-256 (header) | CRC-32 (weak) | SHA-256 (per-module) | None |
| Module types | ES, JSON, text, data | Just files | ES, JSON, Wasm, data | Just strings |
| Manifest readable without decompression | Yes | No (central dir at end) | Yes (header) | No |
| Parse complexity | ~100 LOC | ZIP library | ~200 LOC (or Deno crate) | serde_json |
| Compression quality | zstd (best ratio/speed) | deflate (dated) | None | N/A |
| Transport optimization | HTTP gzip on top | Already compressed | HTTP gzip on top | HTTP gzip |
