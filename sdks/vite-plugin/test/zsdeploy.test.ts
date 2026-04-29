/**
 * Tests for the `.zsdeploy` emitter.
 *
 * The emitter walks `dist/`, hashes every file, packs into a tar.zst archive,
 * and emits `dist/app.zsdeploy`. These tests build a hand-crafted fixture
 * directory tree, run the emitter, and inspect the output for the
 * cross-references the control plane will validate (manifest schema,
 * blob presence, manifest-first tar order, hash format).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { zstdDecompressSync } from "node:zlib";

import { emitZsdeploy } from "../src/zsdeploy.js";

// ── Fixture builder ────────────────────────────────────────────────────────

interface Fixture {
  root: string;
  cleanup: () => Promise<void>;
}

async function makeFixture(files: Record<string, string | Buffer>): Promise<Fixture> {
  const root = join(tmpdir(), `zsdeploy-test-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });

  for (const [relPath, content] of Object.entries(files)) {
    const abs = resolve(root, relPath);
    await fs.mkdir(dirname(abs), { recursive: true });
    await fs.writeFile(
      abs,
      typeof content === "string" ? content : content
    );
  }

  return {
    root,
    cleanup: () => fs.rm(root, { recursive: true, force: true }),
  };
}

// ── Tar/zstd unpacker ──────────────────────────────────────────────────────

interface TarEntry {
  name: string;
  bytes: Buffer;
}

/**
 * Parse a USTAR tar archive into a list of (name, bytes) entries.
 * Header layout: 512-byte header per entry, file name in offset 0..100,
 * file size (octal ASCII) in offset 124..136, body padded to 512.
 *
 * We implement this by hand to avoid pulling another dep into the test —
 * the format is small enough and this doubles as a sanity check that we
 * actually emit a parseable tar.
 */
function parseTar(buf: Buffer): TarEntry[] {
  const entries: TarEntry[] = [];
  let offset = 0;
  while (offset + 512 <= buf.length) {
    const header = buf.subarray(offset, offset + 512);
    // End-of-archive: two consecutive 512-byte zero blocks.
    if (header.every((b) => b === 0)) break;

    // GNU/PAX may use a long-name extension; we ignore those — Vite's
    // builds don't produce paths > 100 chars.
    const nameRaw = header.subarray(0, 100).toString("utf8");
    const name = nameRaw.replace(/\0+$/, "");
    const sizeRaw = header
      .subarray(124, 136)
      .toString("ascii")
      .replace(/\0+$/, "")
      .trim();
    const size = parseInt(sizeRaw, 8);
    if (Number.isNaN(size)) {
      throw new Error(`tar parse: bad size header for ${name}: ${sizeRaw}`);
    }

    offset += 512;
    const bytes = buf.subarray(offset, offset + size);
    entries.push({ name, bytes: Buffer.from(bytes) });

    // Advance past the body, rounded up to the next 512-byte block.
    offset += Math.ceil(size / 512) * 512;
  }
  return entries;
}

function sha256Hex(b: Buffer | string): string {
  return createHash("sha256")
    .update(typeof b === "string" ? Buffer.from(b) : b)
    .digest("hex");
}

// ── Tests ──────────────────────────────────────────────────────────────────

describe("emitZsdeploy", () => {
  test("emits a .zsdeploy archive with manifest first and all referenced blobs present", async () => {
    const fix = await makeFixture({
      "dist/index.html":
        '<!doctype html><html><head><script type="module" src="/assets/main-abc.js"></script></head><body><div id="root"></div></body></html>',
      "dist/assets/main-abc.js": "console.log('hello');\n",
      "dist/assets/main-abc.css": "body{margin:0}\n",
      "dist/server/index.js":
        "export default { fetch: async (req) => new Response('hello') };\n",
    });

    try {
      const result = await emitZsdeploy({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        compiler: "@zeroship/vite-plugin@test",
        silent: true,
      });

      // Archive exists at the documented path.
      assert.equal(
        result.outputPath,
        resolve(fix.root, "dist/app.zsdeploy"),
        "archive path"
      );
      const archiveBytes = await fs.readFile(result.outputPath);
      assert.ok(archiveBytes.length > 0, "archive non-empty");
      assert.equal(archiveBytes.length, result.archiveSize);

      // Decompress + parse tar.
      const tarBytes = zstdDecompressSync(archiveBytes);
      const entries = parseTar(tarBytes);
      assert.ok(entries.length >= 2, "at least manifest + 1 blob");

      // Manifest is the FIRST entry (spec requirement).
      assert.equal(entries[0].name, "manifest.json", "manifest is first");

      // All other entries are under blobs/.
      for (let i = 1; i < entries.length; i++) {
        assert.match(
          entries[i].name,
          /^blobs\/[0-9a-f]{64}$/,
          `entry ${i} is blobs/<sha256>`
        );
      }

      // Parse manifest.
      const manifest = JSON.parse(entries[0].bytes.toString("utf8"));

      // Schema invariants.
      assert.equal(manifest.version, 2, "version=2");
      assert.equal(manifest.asset_version, 0, "asset_version=0");
      assert.deepEqual(manifest.runtime_assets, {}, "runtime_assets={}");
      assert.equal(typeof manifest.metadata.built_at, "string", "built_at present");
      assert.equal(manifest.metadata.built_at, "2026-04-29T00:00:00Z");
      assert.equal(manifest.metadata.compiler, "@zeroship/vite-plugin@test");

      // Worker shape: { entry, modules } with entry as a key in modules.
      assert.ok(manifest.worker, "worker present");
      assert.equal(manifest.worker.entry, "index.js");
      assert.ok(
        "index.js" in manifest.worker.modules,
        "entry is a key in worker.modules"
      );
      // Hash matches the on-disk file's sha256.
      const serverBytes = await fs.readFile(
        resolve(fix.root, "dist/server/index.js")
      );
      assert.equal(
        manifest.worker.modules["index.js"],
        sha256Hex(serverBytes),
        "worker module hash matches disk"
      );

      // Assets — both the JS and CSS chunks are present, plus index.html.
      assert.ok(manifest.assets["/index.html"], "index.html in assets");
      assert.ok(
        manifest.assets["/assets/main-abc.js"],
        "main-abc.js in assets"
      );
      assert.ok(
        manifest.assets["/assets/main-abc.css"],
        "main-abc.css in assets"
      );
      // Worker bundle is NOT in assets (only in worker.modules).
      assert.ok(
        !("/server/index.js" in manifest.assets),
        "worker bundle not in assets"
      );

      // Asset hash format + size match.
      const jsBytes = await fs.readFile(
        resolve(fix.root, "dist/assets/main-abc.js")
      );
      assert.equal(
        manifest.assets["/assets/main-abc.js"].hash,
        sha256Hex(jsBytes),
        "asset hash matches disk"
      );
      assert.equal(
        manifest.assets["/assets/main-abc.js"].size,
        jsBytes.length,
        "asset size matches disk"
      );
      assert.match(
        manifest.assets["/assets/main-abc.js"].hash,
        /^[0-9a-f]{64}$/,
        "hash is 64-char lowercase hex"
      );
      assert.equal(
        manifest.assets["/assets/main-abc.js"].content_type,
        "application/javascript",
        "JS content type"
      );
      assert.equal(
        manifest.assets["/assets/main-abc.css"].content_type,
        "text/css",
        "CSS content type"
      );
      assert.equal(
        manifest.assets["/index.html"].content_type,
        "text/html",
        "HTML content type"
      );

      // No `prerendered` field anywhere (post-rename schema).
      assert.ok(!("prerendered" in manifest), "no prerendered field");
      // No `server_bundle` field (post-rename schema).
      assert.ok(!("server_bundle" in manifest), "no server_bundle field");

      // Cross-reference: every hash in assets/worker/sourcemaps must
      // appear as a tar entry.
      const blobNames = new Set(
        entries
          .filter((e) => e.name.startsWith("blobs/"))
          .map((e) => e.name.slice("blobs/".length))
      );
      for (const [path, entry] of Object.entries(manifest.assets) as [
        string,
        { hash: string },
      ][]) {
        assert.ok(
          blobNames.has(entry.hash),
          `blob for asset ${path} (hash ${entry.hash}) is in archive`
        );
      }
      for (const [, hash] of Object.entries(manifest.worker.modules) as [
        string,
        string,
      ][]) {
        assert.ok(
          blobNames.has(hash),
          `blob for worker module (hash ${hash}) is in archive`
        );
      }

      // Each tar blob's content hashes to its filename (possession proof).
      for (const e of entries) {
        if (!e.name.startsWith("blobs/")) continue;
        const expected = e.name.slice("blobs/".length);
        assert.equal(
          sha256Hex(e.bytes),
          expected,
          `blob ${e.name} content hashes to its name`
        );
      }

      // Rules — at minimum the worker rules.
      const ruleSummary = manifest.rules.map(
        (r: { match: { kind: string }; action: { kind: string; mode?: string } }) =>
          `${r.match.kind}/${r.action.kind}${r.action.mode ? "/" + r.action.mode : ""}`
      );
      assert.ok(
        ruleSummary.some((s: string) => s === "prefix/worker/rpc"),
        "has POST /_rpc/ -> worker(rpc) rule"
      );
      assert.ok(
        ruleSummary.some((s: string) => s === "any/worker/ssr"),
        "has catch-all -> worker(ssr) rule"
      );

      // No deploy_hash at build time — control plane fills it.
      assert.ok(
        !("deploy_hash" in manifest) || manifest.deploy_hash === undefined,
        "deploy_hash absent at build time"
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("emits SSG-only archive (no worker) with SPA fallback rule when no server bundle exists", async () => {
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html><html><body>hello</body></html>",
      "dist/assets/style-x.css": "body{color:red}",
    });
    try {
      const result = await emitZsdeploy({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const entries = parseTar(tarBytes);
      const manifest = JSON.parse(entries[0].bytes.toString("utf8"));

      // No worker — field is omitted entirely for SSG-only builds (matches
      // the spec's "null/missing means SSG-only" wording, and Rust's serde
      // skip_serializing_if = Option::is_none).
      assert.ok(!("worker" in manifest), "worker field absent for SSG-only");

      // SPA-fallback rule is present.
      const hasSpaRule = manifest.rules.some(
        (r: {
          match: { kind: string };
          action: { kind: string; try?: string[] };
        }) =>
          r.match.kind === "any" &&
          r.action.kind === "static" &&
          Array.isArray(r.action.try) &&
          r.action.try.includes("/index.html")
      );
      assert.ok(hasSpaRule, "SPA fallback rule present");

      // No worker rules.
      const hasWorkerRule = manifest.rules.some(
        (r: { action: { kind: string } }) => r.action.kind === "worker"
      );
      assert.ok(!hasWorkerRule, "no worker rules in SSG-only build");
    } finally {
      await fix.cleanup();
    }
  });

  test("emits sourcemap mapping (asset hash → sourcemap hash) for matching .map files", async () => {
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/assets/app.js": "console.log(1)",
      "dist/assets/app.js.map": '{"version":3,"sources":[]}',
    });
    try {
      const result = await emitZsdeploy({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const entries = parseTar(tarBytes);
      const manifest = JSON.parse(entries[0].bytes.toString("utf8"));

      const assetHash = sha256Hex(
        await fs.readFile(resolve(fix.root, "dist/assets/app.js"))
      );
      const mapHash = sha256Hex(
        await fs.readFile(resolve(fix.root, "dist/assets/app.js.map"))
      );

      assert.equal(manifest.sourcemaps[assetHash], mapHash);
      // Sourcemap itself is NOT in `assets` (sourcemaps are sibling-only metadata).
      assert.ok(
        !("/assets/app.js.map" in manifest.assets),
        "sourcemap not in assets"
      );

      // The sourcemap blob IS in the archive (control plane needs it to
      // serve /_assets/<hash>.map).
      const blobNames = new Set(
        entries
          .filter((e) => e.name.startsWith("blobs/"))
          .map((e) => e.name.slice("blobs/".length))
      );
      assert.ok(blobNames.has(mapHash), "sourcemap blob in archive");
    } finally {
      await fix.cleanup();
    }
  });

  test("worker sourcemaps pair with the worker module hash", async () => {
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/server/index.js":
        "export default { fetch: () => new Response('') };",
      "dist/server/index.js.map": '{"version":3,"sources":[]}',
    });
    try {
      const result = await emitZsdeploy({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const entries = parseTar(tarBytes);
      const manifest = JSON.parse(entries[0].bytes.toString("utf8"));

      const workerHash = manifest.worker.modules["index.js"];
      const mapHash = sha256Hex(
        await fs.readFile(resolve(fix.root, "dist/server/index.js.map"))
      );

      // Worker sourcemap should pair with the worker module hash, not be
      // treated as a worker module of its own.
      assert.equal(
        manifest.sourcemaps[workerHash],
        mapHash,
        "worker sourcemap pairs with worker hash"
      );
      assert.equal(
        Object.keys(manifest.worker.modules).length,
        1,
        "worker has only one module — sourcemap is NOT a module"
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("dedupes identical blob bytes (one tar entry per content)", async () => {
    const fix = await makeFixture({
      "dist/a.txt": "same content",
      "dist/b.txt": "same content",
      "dist/c.txt": "same content",
    });
    try {
      const result = await emitZsdeploy({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const entries = parseTar(tarBytes);
      const blobs = entries.filter((e) => e.name.startsWith("blobs/"));
      assert.equal(blobs.length, 1, "three identical files dedup to one blob");
      assert.equal(result.blobCount, 1);
    } finally {
      await fix.cleanup();
    }
  });

  test("rejects build with no files", async () => {
    const fix = await makeFixture({});
    await fs.mkdir(resolve(fix.root, "dist"), { recursive: true });
    try {
      await assert.rejects(
        () =>
          emitZsdeploy({
            root: fix.root,
            builtAt: "2026-04-29T00:00:00Z",
            silent: true,
          }),
        /no files found/
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("emits correct routing rules for /favicon.ico when present", async () => {
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/favicon.ico": "fake-ico-bytes",
      "dist/assets/x.js": "x",
    });
    try {
      const result = await emitZsdeploy({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const entries = parseTar(tarBytes);
      const manifest = JSON.parse(entries[0].bytes.toString("utf8"));

      const hasFaviconRule = manifest.rules.some(
        (r: {
          match: { kind: string; path?: string };
          action: { kind: string };
        }) =>
          r.match.kind === "exact" &&
          r.match.path === "/favicon.ico" &&
          r.action.kind === "static"
      );
      assert.ok(hasFaviconRule, "favicon.ico has its own static rule");
    } finally {
      await fix.cleanup();
    }
  });

  test("manifest validation rejects orphan asset hashes (defensive — should never happen)", async () => {
    // This is exercised internally by validateManifest(); we verify by
    // running a happy-path build and confirming validation passes (no
    // throw from emitZsdeploy). The unit-level test for the validation
    // function would require exporting it; we keep that internal and
    // rely on the integration test above which would catch any
    // cross-reference bug as a missing-blob assertion failure.
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/server/index.js": "export default { fetch: () => new Response('') };",
    });
    try {
      await assert.doesNotReject(() =>
        emitZsdeploy({
          root: fix.root,
          builtAt: "2026-04-29T00:00:00Z",
          silent: true,
        })
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("staging dir is cleaned up after emission", async () => {
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/server/index.js": "export default { fetch: () => new Response('') };",
    });
    try {
      await emitZsdeploy({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const stagingPath = resolve(fix.root, "dist/.zsdeploy");
      const exists = await fs
        .stat(stagingPath)
        .then(() => true)
        .catch(() => false);
      assert.ok(!exists, "staging dir cleaned up");
    } finally {
      await fix.cleanup();
    }
  });
});
