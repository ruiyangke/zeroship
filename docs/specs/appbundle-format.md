# .appbundle Format Specification

**Version:** 1
**Date:** 2026-04-09
**Status:** Draft

## Overview

`.appbundle` is a binary format for packaging JavaScript modules for the appbase runtime. It stores multiple ES modules with per-module zstd compression and a binary index for O(1) lookup by specifier.

The bundle carries only code and integrity. All metadata (app name, version, exports, compiler) lives in the database.

## Design goals

1. **O(1) module lookup** — binary index maps specifier → offset, no scanning
2. **Lazy decompression** — only imported modules are decompressed
3. **Integrity** — SHA-256 verifiable before parsing any module
4. **No escaping** — module sources stored as raw bytes
5. **Minimal** — no manifest, no metadata, no JSON; just an index and data
6. **Simple** — parseable in ~50 lines, writable in ~50 lines

## Binary layout

```
┌──────────────────────────────────────────────┐
│ Header (40 bytes, fixed)                     │
│  Magic: "APPB" (4 bytes, ASCII)              │
│  Format version: u32 big-endian (4 bytes)    │
│  SHA-256 hash of all bytes after (32 bytes)  │
├──────────────────────────────────────────────┤
│ Index Section                                │
│  Module count: u32 big-endian (4 bytes)      │
│  Index entry × N (variable)                  │
├──────────────────────────────────────────────┤
│ Data Section                                 │
│  Concatenated zstd frames (variable)         │
└──────────────────────────────────────────────┘
```

All multi-byte integers are unsigned big-endian.

## Header

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 4 | Magic: ASCII `APPB` |
| 4 | 4 | Format version: `1` |
| 8 | 32 | SHA-256 of bytes 40 → EOF |

Readers must reject files where magic ≠ `APPB` or version > supported.

The hash covers the index and data sections. Verification: hash bytes 40 → EOF, compare with bytes 8–39. This catches corruption, truncation, and tampering before any module is parsed.

## Index section

Starts at byte 40. Contains a module count followed by N index entries.

| Size | Field |
|-----:|-------|
| 4 | Module count (N) |

Each index entry:

| Size | Field |
|-----:|-------|
| 2 | Specifier length (S) |
| S | Specifier (UTF-8, e.g. `index.js`, `utils/helper.js`) |
| 1 | Module type |
| 4 | Data offset — relative to start of data section |
| 4 | Compressed source size |
| 4 | Original source size (for pre-allocating decompression buffer) |
| 4 | Compressed source map size (0 = no source map) |
| 4 | Original source map size (0 = no source map) |

The **first entry** in the index is the entry module. The runtime uses this as the starting point for module evaluation.

Module types:

| Value | Type | V8 loading |
|:-----:|------|------------|
| 0 | ES module | `compile_module` with `is_module = true` |
| 1 | JSON | Import produces a JSON object |
| 2 | Text | Import produces a string |
| 3 | Binary data | Import produces an ArrayBuffer |

## Data section

Starts immediately after the last index entry. Contains concatenated zstd-compressed frames in the order:

```
[module 0 source] [module 0 source map]
[module 1 source] [module 1 source map]
...
```

Source map frames are omitted when compressed source map size = 0 in the index entry.

Each frame is an independent zstd frame. Modules can be decompressed individually without touching other modules.

## Reading

1. Read bytes 0–7: verify magic and version
2. Read bytes 8–39: store expected hash
3. SHA-256 hash bytes 40 → EOF: compare with expected hash, reject on mismatch
4. Read module count at byte 40
5. Read N index entries sequentially, build `HashMap<specifier, IndexEntry>`
6. Record the byte position after the last index entry — this is the data section start

To load a module by specifier:
1. Look up specifier in HashMap → get `data_offset` and `compressed_size`
2. Seek to `data_section_start + data_offset`
3. Read `compressed_size` bytes
4. zstd decompress into pre-allocated buffer of `original_size` bytes
5. Return as UTF-8 string

## Writing

Two-pass process:

**Pass 1 — compress:**
1. For each module: zstd compress source, optionally compress source map
2. Record compressed and original sizes

**Pass 2 — compute offsets and write:**
1. Compute data offsets: cumulative sum of compressed sizes (sources + source maps)
2. Serialize index entries with computed offsets
3. Concatenate: header (with placeholder hash) + index + all compressed data
4. SHA-256 hash bytes 40 → EOF
5. Write hash into bytes 8–39

## What is NOT in the bundle

These belong in the database, not the bundle:

| Field | Why not in bundle |
|-------|-------------------|
| App name | Routing/management concern, not code |
| Version number | Database tracks deploy history |
| Exports list | Unreliable (JS is dynamic); runtime discovers exports by evaluation |
| Compiler version | Build metadata, not needed at runtime |
| Compatibility date | Runtime versioning policy, not per-bundle |
| API key | Security — never bundle credentials |

The bundle is **pure code + integrity**. Everything else is the platform's responsibility.

## Lazy decompression + lazy compilation

The format enables two levels of laziness:

**Level 1 — lazy decompression:** The index is read eagerly (small, ~23 bytes per module). Module sources stay as compressed bytes. Only decompressed when the runtime needs them.

**Level 2 — lazy compilation:** The runtime uses `v8::Module::get_module_requests()` to discover imports from compiled modules. Only transitively imported modules are decompressed and compiled. Unused modules are never touched.

Combined effect for a 200-module app where the entry imports 5 modules transitively:

| Stage | Modules touched | Memory |
|-------|----------------:|-------:|
| After index parse | 0 of 200 | ~5KB (index) + blob |
| After entry compiled | 1 of 200 | + entry decompressed |
| After imports compiled | 5 of 200 | + 4 imports decompressed |
| Serving requests | 5 of 200 | 195 modules untouched |

## Size estimates

| App type | Modules | Uncompressed | .appbundle size | In-memory (5 imported) |
|----------|--------:|-------------:|----------------:|-----------------------:|
| Simple API | 1 | 5 KB | ~2 KB | 5 KB |
| Medium app | 10 | 50 KB | ~18 KB | ~25 KB |
| Large (bundled) | 1 | 500 KB | ~110 KB | 500 KB |
| Large (unbundled) | 200 | 2 MB | ~520 KB | ~130 KB |

## Comparison

| | .appbundle | ZIP | eszip | Raw JS in DB |
|---|---|---|---|---|
| Module lookup | O(1) index | O(N) scan or central dir | O(1) header | N/A (single string) |
| Per-module compression | zstd | deflate | None | None |
| Lazy decompression | Yes | No | Yes (streaming) | N/A |
| Integrity | SHA-256 | CRC-32 | SHA-256 / XXHash3 | None |
| Module types | ES, JSON, text, data | Just files | ES, JSON, Wasm, data | ES only |
| Format complexity | ~50 LOC | ZIP library | ~200 LOC | 0 |
| Metadata in bundle | None | Optional | Optional | None |
