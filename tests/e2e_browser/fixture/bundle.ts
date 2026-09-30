import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { zstdDecompressSync } from "node:zlib";
import { Parser } from "tar";

interface Resource { auth?: string; static?: { try: string[] } }

export interface Manifest {
  worker?: unknown;
  assets: Record<string, unknown>;
  resources: Record<string, Resource>;
}

/** The manifest a `.zship` leads with. */
export async function readManifest(bundle: string): Promise<Manifest> {
  let first: string | undefined;
  const chunks: Buffer[] = [];
  const parser = new Parser({ onReadEntry(entry) {
    first ??= entry.path;
    if (entry.path === "manifest.json") entry.on("data", (chunk: Buffer) => chunks.push(chunk));
    else entry.resume();
  } });
  await pipeline(Readable.from([zstdDecompressSync(await readFile(bundle))]), parser);
  assert.equal(first, "manifest.json", `${bundle} must lead with manifest.json`);
  return JSON.parse(Buffer.concat(chunks).toString()) as Manifest;
}

/**
 * An SSG deploy carries no worker and serves every route from its assets.
 * The browser cannot tell that apart from a worker that returns the same
 * HTML, so the SSG specs rest on this check of the manifest the build wrote.
 * `pages` are the documents the specs load; requiring them keeps the check
 * from passing over a manifest with nothing in it.
 */
export function assertStaticOnly(manifest: Manifest, pages: string[], where: string): void {
  assert.equal(manifest.worker ?? null, null, `${where}: the manifest declares a worker`);
  for (const page of pages) assert(page in manifest.assets, `${where}: the manifest ships no ${page}`);
  const routes = Object.entries(manifest.resources);
  assert(routes.length > 0, `${where}: the manifest declares no routes`);
  for (const [path, resource] of routes) {
    assert(resource.static, `${where}: route ${path} is not served from assets`);
  }
}
