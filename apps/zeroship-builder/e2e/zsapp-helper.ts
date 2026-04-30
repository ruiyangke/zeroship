// ─── e2e helper: build a minimal `.zsapp` archive in-memory ─────
//
// Mirrors the wire format documented in `docs/reference/zsapp.md`
// (schema v2). Only what tests need: a single-module worker, no
// static assets, the same routing rules `Manifest::passthrough()`
// would synthesize on the server.
//
// The full build pipeline lives in `sdks/vite-plugin/src/zsapp.ts`;
// inlining a minimal USTAR encoder + zstd compress here keeps the
// e2e suite self-contained (no extra `tar` dep on the builder app).

import { createHash } from "node:crypto";
import { zstdCompressSync } from "node:zlib";

/**
 * Pack a single-module worker bundle into a `.zsapp` archive. The
 * resulting Buffer is the body to POST as `Content-Type:
 * application/x-zsapp` to `/api/apps/{id}/deploy`.
 *
 * @param serverJs the worker module source (becomes `index.js`).
 */
export function buildZsapp(serverJs: string): Buffer {
  const blobBytes = Buffer.from(serverJs, "utf8");
  const hash = sha256Hex(blobBytes);

  // Manifest: no assets, two rules (`POST /_rpc/* → rpc`, `* → ssr`),
  // one worker module. Matches the shape `Manifest::passthrough()`
  // would synthesize.
  const manifest = {
    version: 2,
    rules: [
      {
        action: { kind: "worker", mode: "rpc" },
        match: { kind: "prefix", method: "POST", path: "/_rpc/" },
      },
      {
        action: { kind: "worker", mode: "ssr" },
        match: { kind: "any" },
      },
    ],
    assets: {},
    runtime_assets: {},
    asset_version: 0,
    sourcemaps: {},
    worker: {
      entry: "index.js",
      modules: { "index.js": hash },
    },
    metadata: {
      compiler: "e2e-test-fixture",
      built_at: new Date().toISOString(),
    },
  };
  const manifestBytes = Buffer.from(JSON.stringify(manifest), "utf8");

  // USTAR-encoded tar: manifest first, then `blobs/<hash>`. The
  // server requires manifest.json as the very first entry.
  const tarBytes = encodeTar([
    { name: "manifest.json", bytes: manifestBytes },
    { name: `blobs/${hash}`, bytes: blobBytes },
  ]);

  return zstdCompressSync(tarBytes);
}

function sha256Hex(bytes: Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}

interface TarEntry {
  name: string;
  bytes: Buffer;
}

/**
 * Minimal USTAR (POSIX 1003.1-1988) tar encoder. Each entry: 512-byte
 * header + body padded to 512-byte alignment. Two trailing zero-blocks
 * mark end-of-archive.
 *
 * Names must be ≤ 100 chars (we don't bother with USTAR's prefix
 * field) — `blobs/<64-char-sha256>` is 70 chars, well within limit.
 */
function encodeTar(entries: TarEntry[]): Buffer {
  const chunks: Buffer[] = [];
  for (const e of entries) {
    if (Buffer.byteLength(e.name, "utf8") > 100) {
      throw new Error(`tar: entry name too long: ${e.name}`);
    }
    chunks.push(buildHeader(e.name, e.bytes.length));
    chunks.push(e.bytes);
    const pad = (512 - (e.bytes.length % 512)) % 512;
    if (pad > 0) chunks.push(Buffer.alloc(pad));
  }
  // End-of-archive: two consecutive zero blocks.
  chunks.push(Buffer.alloc(1024));
  return Buffer.concat(chunks);
}

function buildHeader(name: string, size: number): Buffer {
  const h = Buffer.alloc(512);
  h.write(name, 0, 100, "utf8");                            // name[100]
  h.write("0000644\0", 100, 8, "ascii");                    // mode (octal)
  h.write("0000000\0", 108, 8, "ascii");                    // uid
  h.write("0000000\0", 116, 8, "ascii");                    // gid
  h.write(size.toString(8).padStart(11, "0") + "\0", 124, 12, "ascii"); // size
  h.write("00000000000\0", 136, 12, "ascii");               // mtime (0 → reproducible)
  h.write("        ", 148, 8, "ascii");                     // chksum placeholder (8 spaces)
  h.write("0", 156, 1, "ascii");                            // typeflag '0' = regular file
  // linkname[100], magic[6], version[2], uname[32], gname[32],
  // devmajor[8], devminor[8], prefix[155] all left zero.
  h.write("ustar\0", 257, 6, "ascii");                      // magic
  h.write("00", 263, 2, "ascii");                           // version

  // Checksum: the unsigned sum of all 512 bytes with the chksum field
  // treated as 8 spaces. Stored as 6-octal-digit + NUL + space.
  let sum = 0;
  for (let i = 0; i < 512; i++) sum += h[i];
  const chk = sum.toString(8).padStart(6, "0") + "\0 ";
  h.write(chk, 148, 8, "ascii");
  return h;
}
