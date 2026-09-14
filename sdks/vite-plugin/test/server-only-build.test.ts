/**
 * ISS-59 regression — a SERVER-ONLY app (no client `index.html`, just
 * `default = { fetch?, rpc? }` plus RPC procedures) MUST build
 * to a valid `.zship`.
 *
 * Root cause (pre-fix): in `"full"` mode the SSR sub-build is triggered
 * from the client environment's `writeBundle` hook. With zero client JS
 * inputs (no `index.html`, no `rollupOptions.input`) Vite/rolldown emits
 * no output bundle, so `writeBundle` NEVER fires — the SSR build is
 * skipped and `dist/` is never created. `closeBundle` still runs and
 * calls `emitZship`, which throws `zship: dist dir not found`.
 *
 * This test runs a REAL `vite build` with the full `zeroship()` plugin
 * chain on a server-only fixture and asserts:
 *   - the build succeeds
 *   - `dist/app.zship` exists
 *   - the manifest has a `worker` with an `index.js` entry (the RPC
 *     dispatcher), and
 *   - the auto-derived RPC procedure resources are present.
 *
 * Pre-fix this test FAILS at `vite build` with the dist-dir error.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { zstdDecompressSync } from "node:zlib";
import { build as viteBuild } from "vite";

import { zeroship } from "../src/index.js";

// ── Minimal USTAR reader (manifest.json is the first entry) ────────────────

function readZship(archive: Buffer): {
  manifest: Record<string, unknown>;
  entries: Set<string>;
  bodies: Map<string, Buffer>;
} {
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

// A pure-backend app: `default = { fetch }`, NO `index.html`, NO client
// entry. The runtime dispatches via the bundled worker. Minimal local
// framework packages keep the fixture isolated while exercising procedure
// discovery when no client graph has already transformed the server entry.
const SERVER_TS = `
"use server";
import { query } from "@zeroship/rpc/server";

export const ping = query(async () => "pong", { id: "probe.ping" });

export default {
  fetch(req) {
    return new Response("hello from a server-only app");
  },
};
`;

async function makeServerOnlyApp(): Promise<string> {
  const root = join(tmpdir(), `zs-server-only-${randomUUID()}`);
  await fs.mkdir(resolve(root, "src"), { recursive: true });
  await fs.mkdir(resolve(root, "node_modules/@zeroship/rpc"), { recursive: true });
  await fs.mkdir(resolve(root, "node_modules/@zeroship/server"), { recursive: true });
  await fs.writeFile(resolve(root, "src", "server.ts"), SERVER_TS, "utf8");
  await fs.writeFile(
    resolve(root, "node_modules/@zeroship/rpc/package.json"),
    JSON.stringify({ type: "module", exports: { "./server": "./server.js" } }),
  );
  await fs.writeFile(
    resolve(root, "node_modules/@zeroship/rpc/server.js"),
    `export function query(handler, config = {}) {
      Object.defineProperty(handler, "config", {
        value: { ...config, kind: "query" },
        enumerable: true,
        configurable: true,
      });
      return handler;
    }`,
  );
  await fs.writeFile(
    resolve(root, "node_modules/@zeroship/server/package.json"),
    JSON.stringify({ type: "module", exports: "./index.js" }),
  );
  await fs.writeFile(
    resolve(root, "node_modules/@zeroship/server/index.js"),
    `export function __makeServerProcedure(handler, metadata) {
      return Object.assign((input) => handler(input), metadata);
    }`,
  );
  // Deliberately NO index.html and NO rollupOptions.input — this is the
  // exact shape that broke ISS-59.
  return root;
}

describe("ISS-59 — server-only app (no index.html) builds to a valid .zship", () => {
  test("vite build succeeds and emits a worker + SSR catch-all", async () => {
    const root = await makeServerOnlyApp();
    try {
      await viteBuild({
        root,
        configFile: false,
        logLevel: "silent",
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        plugins: zeroship() as any,
      });

      const archivePath = resolve(root, "dist", "app.zship");
      // Pre-fix: vite build throws before we get here. If it somehow
      // produced no archive, this assertion still pins the contract.
      const archive = await fs.readFile(archivePath);
      const { manifest, bodies } = readZship(archive);

      assert.equal(manifest.version, 1, "manifest schema version");

      // A server-only RPC app ships a worker — the bundled dispatcher.
      const worker = manifest.worker as
        | { entry: string; modules: Record<string, string> }
        | undefined
        | null;
      assert.ok(worker, "manifest.worker must be present for a backend app");
      assert.equal(worker.entry, "index.js", "worker entry is index.js");
      assert.ok(
        worker.entry in worker.modules,
        "worker.entry is a key in worker.modules",
      );
      const workerHash = worker.modules[worker.entry];
      const workerBody = bodies.get(`blobs/${workerHash}`);
      assert.ok(workerBody, "worker module body must be packed");
      const workerPath = resolve(root, "worker.mjs");
      await fs.writeFile(workerPath, workerBody);
      const loaded = await import(pathToFileURL(workerPath).href);
      assert.deepEqual(Object.keys(loaded.default.rpc), ["probe.ping"]);
      assert.equal(await loaded.default.rpc["probe.ping"](), "pong");

      // No client SPA shell exists, but the worker exports `default.fetch`,
      // so the catch-all forwards every unmatched URL to the worker (SSR).
      const resources = (manifest.resources ?? {}) as Record<
        string,
        Record<string, unknown>
      >;
      const catchAll = resources["/[...rest]"];
      assert.ok(resources["rpc:probe.ping"], "expected the RPC resource");
      assert.ok(catchAll, "expected an SSR catch-all resource");
      // A worker(SSR) catch-all has NO `static` action — it falls through
      // to worker dispatch (marked anonymous + publicly_accessible).
      assert.equal(
        catchAll.static,
        undefined,
        "SSR catch-all must not be a static action",
      );
      assert.equal(catchAll.publicly_accessible, true);

      // And critically: there is NO client SPA shell — this was a
      // server-only build (no index.html).
      const assets = (manifest.assets ?? {}) as Record<string, unknown>;
      assert.equal(
        assets["/index.html"],
        undefined,
        "server-only build must not emit an /index.html asset",
      );
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("custom build.dist packs the full server artifact", async () => {
    const root = await makeServerOnlyApp();
    try {
      await fs.writeFile(resolve(root, "index.html"), "<!doctype html><title>custom dist</title>\n");
      await fs.writeFile(
        resolve(root, "zeroship.jsonc"),
        JSON.stringify({
          name: "custom-dist",
          control: "http://localhost:9090",
          runtime_date: "2042-03-04",
          build: { mode: "full", dist: "build", output: "build/app.zship" },
          migrations: { dir: "migrations", out: "generated/zeroship" },
        }),
      );

      await viteBuild({
        root,
        configFile: false,
        logLevel: "silent",
        build: { outDir: "build" },
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        plugins: zeroship() as any,
      });

      const archive = await fs.readFile(resolve(root, "build", "app.zship"));
      const { manifest, entries } = readZship(archive);
      assert.equal(
        manifest.runtime_date,
        "2042-03-04",
        "the archive must carry the configured inert runtime date",
      );
      const worker = manifest.worker as
        | { entry: string; modules: Record<string, string> }
        | undefined
        | null;
      assert.ok(worker, "a mode=full custom-dist archive must contain its server worker");
      assert.equal(worker.entry, "index.js");
      const workerHash = worker.modules[worker.entry];
      assert.match(workerHash, /^[0-9a-f]{64}$/);
      assert.ok(
        entries.has(`blobs/${workerHash}`),
        "the packed archive must contain the worker module blob",
      );
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });
});
