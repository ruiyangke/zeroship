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
import { randomUUID } from "node:crypto";
import { zstdDecompressSync } from "node:zlib";
import { build as viteBuild } from "vite";

import { zeroship } from "../src/index.js";

// ── Minimal USTAR reader (manifest.json is the first entry) ────────────────

function readManifestFromZship(archive: Buffer): Record<string, unknown> {
  const tar = zstdDecompressSync(archive);
  let offset = 0;
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
    if (name === "manifest.json") {
      return JSON.parse(body.toString("utf8"));
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  throw new Error("manifest.json not found in archive");
}

// A pure-backend app: `default = { fetch }`, NO `index.html`, NO client
// entry. The runtime dispatches via the bundled worker. We intentionally
// keep this dependency-free (no `@zeroship/*` imports) so the fixture
// resolves from an isolated tmp root — the bug under test is purely about
// the SSR build never running when there's no client output, independent
// of which procedures the entry declares.
const SERVER_TS = `
export default {
  fetch(req) {
    return new Response("hello from a server-only app");
  },
};
`;

async function makeServerOnlyApp(): Promise<string> {
  const root = join(tmpdir(), `zs-server-only-${randomUUID()}`);
  await fs.mkdir(resolve(root, "src"), { recursive: true });
  await fs.writeFile(resolve(root, "src", "server.ts"), SERVER_TS, "utf8");
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
      const manifest = readManifestFromZship(archive);

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

      // No client SPA shell exists, but the worker exports `default.fetch`,
      // so the catch-all forwards every unmatched URL to the worker (SSR).
      const resources = (manifest.resources ?? {}) as Record<
        string,
        Record<string, unknown>
      >;
      const catchAll = resources["/[...rest]"];
      assert.ok(catchAll, "expected an SSR catch-all resource");
      // A worker(SSR) catch-all has NO `static` action — it falls through
      // to worker dispatch (marked anon + publicly_accessible).
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
});
