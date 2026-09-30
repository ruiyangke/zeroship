// Reads a `.zship` archive back: zstd over USTAR, `manifest.json` first.

import { zstdDecompressSync } from "node:zlib";

export interface ZshipContents {
  manifest: Record<string, unknown>;
  entries: Set<string>;
  bodies: Map<string, Buffer>;
}

export function readZship(archive: Buffer): ZshipContents {
  const tar = zstdDecompressSync(archive);
  let offset = 0;
  let manifest: Record<string, unknown> | undefined;
  const entries = new Set<string>();
  const bodies = new Map<string, Buffer>();
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((b) => b === 0)) break;
    const name = header.subarray(0, 100).toString("utf8").replace(/\0.*$/, "");
    const sizeOctal = header
      .subarray(124, 136)
      .toString("utf8")
      .replace(/\0.*$/, "")
      .trim();
    const size = parseInt(sizeOctal, 8) || 0;
    const body = tar.subarray(offset + 512, offset + 512 + size);
    entries.add(name);
    bodies.set(name, body);
    if (name === "manifest.json") {
      manifest = JSON.parse(body.toString("utf8"));
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  if (manifest == null) throw new Error("manifest.json not found in archive");
  return { manifest, entries, bodies };
}

/** The packed worker: its entry name and module bodies, keyed by module path. */
export function workerModules(contents: ZshipContents): {
  entry: string;
  modules: Map<string, Buffer>;
} {
  const worker = contents.manifest.worker as
    | { entry: string; modules: Record<string, string> }
    | null
    | undefined;
  if (worker == null) throw new Error("the archive packs no worker");
  const modules = new Map<string, Buffer>();
  for (const [path, hash] of Object.entries(worker.modules)) {
    const body = contents.bodies.get(`blobs/${hash}`);
    if (body == null) throw new Error(`worker module ${path} has no packed blob`);
    modules.set(path, body);
  }
  return { entry: worker.entry, modules };
}
