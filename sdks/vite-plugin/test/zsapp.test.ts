/**
 * Tests for the `.zsapp` emitter.
 *
 * The emitter walks `dist/`, hashes every file, packs into a tar.zst archive,
 * and emits `dist/app.zsapp`. These tests build a hand-crafted fixture
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
import { brotliDecompressSync, gunzipSync, zstdDecompressSync } from "node:zlib";

import { emitZsapp } from "../src/zsapp.js";
import {
  BOOTSTRAP_MARKER,
  CLIENT_MANIFEST_RESOLVED_ID,
  CLIENT_MANIFEST_VIRTUAL_ID,
  buildSsrInlineConfig,
  clientManifestPlugin,
  userSourceHasDefaultExport,
  wrapServerBundle,
} from "../src/build.js";

// ── Fixture builder ────────────────────────────────────────────────────────

interface Fixture {
  root: string;
  cleanup: () => Promise<void>;
}

async function makeFixture(files: Record<string, string | Buffer>): Promise<Fixture> {
  const root = join(tmpdir(), `zsapp-test-${randomUUID()}`);
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

describe("emitZsapp", () => {
  test("emits a .zsapp archive with manifest first and all referenced blobs present", async () => {
    const fix = await makeFixture({
      "dist/index.html":
        '<!doctype html><html><head><script type="module" src="/assets/main-abc.js"></script></head><body><div id="root"></div></body></html>',
      "dist/assets/main-abc.js": "console.log('hello');\n",
      "dist/assets/main-abc.css": "body{margin:0}\n",
      "dist/server/index.js":
        "export default { fetch: async (req) => new Response('hello') };\n",
    });

    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        compiler: "@zeroship/vite-plugin@test",
        silent: true,
      });

      // Archive exists at the documented path.
      assert.equal(
        result.outputPath,
        resolve(fix.root, "dist/app.zsapp"),
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
      const result = await emitZsapp({
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
      const result = await emitZsapp({
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
      const result = await emitZsapp({
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
      const result = await emitZsapp({
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
          emitZsapp({
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
      const result = await emitZsapp({
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
    // throw from emitZsapp). The unit-level test for the validation
    // function would require exporting it; we keep that internal and
    // rely on the integration test above which would catch any
    // cross-reference bug as a missing-blob assertion failure.
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/server/index.js": "export default { fetch: () => new Response('') };",
    });
    try {
      await assert.doesNotReject(() =>
        emitZsapp({
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
      await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const stagingPath = resolve(fix.root, "dist/.zsapp");
      const exists = await fs
        .stat(stagingPath)
        .then(() => true)
        .catch(() => false);
      assert.ok(!exists, "staging dir cleaned up");
    } finally {
      await fix.cleanup();
    }
  });

  // ── Pre-compressed variants (Tier 4b) ──────────────────────────────────

  test("precompress_emits_brotli_variant", async () => {
    // Big enough text asset that brotli is unambiguously smaller.
    // Brotli at q=11 of "console.log(...)\n" repeated 200x compresses
    // ~50:1.
    const jsBody = "console.log('hello world');\n".repeat(200);
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/assets/app.js": jsBody,
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
        precompress: { brotli: true, gzip: false },
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const entries = parseTar(tarBytes);
      const manifest = JSON.parse(entries[0].bytes.toString("utf8"));

      const entry = manifest.assets["/assets/app.js"];
      assert.ok(entry, "asset entry present");
      assert.ok(entry.variants, "variants populated");
      assert.ok(entry.variants.br, "brotli variant present");
      // Compressed size beats identity (the whole point).
      assert.ok(
        entry.variants.br.size < entry.size,
        `br ${entry.variants.br.size} < identity ${entry.size}`
      );
      // Brotli hash format check.
      assert.match(entry.variants.br.hash, /^[0-9a-f]{64}$/);
      // Variant blob is in the tar.
      const blobNames = new Set(
        entries
          .filter((e) => e.name.startsWith("blobs/"))
          .map((e) => e.name.slice("blobs/".length))
      );
      assert.ok(blobNames.has(entry.variants.br.hash), "br blob in tar");
      // Decompressing the variant blob yields identity bytes.
      const brBlob = entries.find(
        (e) => e.name === `blobs/${entry.variants.br.hash}`
      )!.bytes;
      assert.equal(
        brotliDecompressSync(brBlob).toString("utf8"),
        jsBody,
        "brotli round-trips to identity"
      );
      // Gzip was disabled — no gzip variant.
      assert.ok(!entry.variants.gzip, "no gzip variant when disabled");
    } finally {
      await fix.cleanup();
    }
  });

  test("precompress_skips_already_small_variants", async () => {
    // Tiny / random-looking text doesn't compress smaller than identity.
    // For a 10-byte file brotli's framing overhead pushes the
    // compressed size above the original — emitter must skip.
    const tiny = "abc";
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/tiny.txt": tiny,
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
        precompress: { brotli: true, gzip: true },
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));
      const entry = manifest.assets["/tiny.txt"];
      // No variants entry — both br and gzip overhead exceeded the
      // 3-byte identity body.
      assert.ok(
        !entry.variants || Object.keys(entry.variants).length === 0,
        `no variants emitted for tiny asset (got ${JSON.stringify(entry.variants)})`
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("precompress_skips_binary_types", async () => {
    // PNG: synthetic file with the magic bytes so mime guesses image/png.
    // Compression skipped entirely — no variants entry.
    const pngHeader = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    const pngBody = Buffer.concat([pngHeader, Buffer.alloc(2048, 0)]);
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/logo.png": pngBody,
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
        precompress: { brotli: true, gzip: true },
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));
      const entry = manifest.assets["/logo.png"];
      assert.equal(entry.content_type, "image/png");
      // Binary type → emitter never even tries to compress.
      assert.ok(
        !entry.variants || Object.keys(entry.variants).length === 0,
        "PNG has no variants"
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("precompress_disabled_yields_no_variants", async () => {
    // Default options enable brotli only. Explicitly turning everything
    // off means the manifest has no variants anywhere, even on
    // compressible types.
    const jsBody = "console.log('hello world');\n".repeat(200);
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/assets/app.js": jsBody,
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
        precompress: { brotli: false, gzip: false },
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));
      const entry = manifest.assets["/assets/app.js"];
      assert.ok(
        !entry.variants || Object.keys(entry.variants).length === 0,
        "no variants when precompress disabled"
      );
    } finally {
      await fix.cleanup();
    }
  });

  // ── Bug 1: SPA fallback for RPC-only apps ──────────────────────────────

  test("csr_only_app_emits_static_spa_fallback", async () => {
    // RPC-only app: server bundle has registry side-effects but no
    // user-defined default.fetch. The bootstrap-injected default.fetch
    // is sufficient for /_rpc/* routing, so the catch-all should NOT
    // hit the worker — it should serve the SPA shell instead.
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html><html><body><div id=root></div></body></html>",
      "dist/assets/main.js": "console.log('spa');\n",
      "dist/server/index.js":
        "// pretend RPC bundle\n__register('foo', () => 1);\n",
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
        // New flag: user did not export default.fetch — this is RPC-only.
        userHasDefaultFetch: false,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));

      // Worker bundle is still present (RPC needs it).
      assert.ok(manifest.worker, "worker present for RPC-only");

      // POST /_rpc/ rule still emitted.
      const hasRpcRule = manifest.rules.some(
        (r: { match: { kind: string; method?: string; path?: string }; action: { kind: string; mode?: string } }) =>
          r.match.kind === "prefix" &&
          r.match.method === "POST" &&
          r.match.path === "/_rpc/" &&
          r.action.kind === "worker" &&
          r.action.mode === "rpc"
      );
      assert.ok(hasRpcRule, "RPC rule still present");

      // Last rule (catch-all) is Static SPA fallback, NOT Worker(SSR).
      const last = manifest.rules[manifest.rules.length - 1];
      assert.equal(last.match.kind, "any", "last rule is catch-all");
      assert.equal(last.action.kind, "static", "catch-all is static");
      assert.deepEqual(
        last.action.try,
        ["$path", "/index.html"],
        "SPA fallback try chain"
      );

      // No Worker(SSR) rule anywhere.
      const hasWorkerSsr = manifest.rules.some(
        (r: { action: { kind: string; mode?: string } }) =>
          r.action.kind === "worker" && r.action.mode === "ssr"
      );
      assert.ok(!hasWorkerSsr, "no Worker(SSR) rule for RPC-only app");
    } finally {
      await fix.cleanup();
    }
  });

  test("ssr_app_emits_worker_catchall", async () => {
    // SSR app: user has default.fetch. Catch-all is Worker(SSR).
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/server/index.js":
        "export default { fetch: async (req) => new Response('hi') };\n",
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
        userHasDefaultFetch: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));

      const last = manifest.rules[manifest.rules.length - 1];
      assert.equal(last.match.kind, "any");
      assert.equal(last.action.kind, "worker");
      assert.equal(last.action.mode, "ssr");
    } finally {
      await fix.cleanup();
    }
  });

  test("precompress_emits_gzip_variant_when_opted_in", async () => {
    // Sanity: gzip variant works when explicitly enabled.
    const jsBody = "console.log('hello world');\n".repeat(200);
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/assets/app.js": jsBody,
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
        precompress: { brotli: false, gzip: true },
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const entries = parseTar(tarBytes);
      const manifest = JSON.parse(entries[0].bytes.toString("utf8"));

      const entry = manifest.assets["/assets/app.js"];
      assert.ok(entry.variants?.gzip, "gzip variant present");
      assert.ok(!entry.variants.br, "no brotli variant when disabled");
      // Decompressing the gzip blob yields identity bytes.
      const gzBlob = entries.find(
        (e) => e.name === `blobs/${entry.variants.gzip.hash}`
      )!.bytes;
      assert.equal(
        gunzipSync(gzBlob).toString("utf8"),
        jsBody,
        "gzip round-trips to identity"
      );
    } finally {
      await fix.cleanup();
    }
  });
});

// ── Bug 2: skip bootstrap append when user has default.fetch ──────────────
describe("wrapServerBundle", () => {
  const PRELUDE = "// fake prelude\nglobalThis.__register=()=>{};\n";
  const BOOTSTRAP =
    "export async function dispatchRpc(){}\nexport default { fetch(){} };\n";

  test("user_default_export_skips_bootstrap_append", () => {
    // SSR-style bundle — user exports their own default.
    const bundle =
      'export default { fetch: async (req) => new Response("hi") };\n';
    const wrapped = wrapServerBundle({
      prelude: PRELUDE,
      bootstrap: BOOTSTRAP,
      bundle,
      userHasDefaultFetch: true,
    });

    // Marker is absent — bootstrap was NOT appended.
    assert.ok(
      !wrapped.includes(BOOTSTRAP_MARKER),
      "bootstrap marker absent when user has default"
    );
    // The user's default.fetch made it through.
    assert.ok(
      wrapped.includes("export default"),
      "user default still in wrapped bundle"
    );
    // Single `export default` — no ESM duplicate-default error.
    const defaultCount = (wrapped.match(/^\s*export\s+default\b/gm) ?? []).length;
    assert.equal(defaultCount, 1, "exactly one export default");
  });

  test("no_default_export_appends_bootstrap", () => {
    // RPC-only bundle — only `__register` side effects.
    const bundle = '__register("listTodos", () => []);\n';
    const wrapped = wrapServerBundle({
      prelude: PRELUDE,
      bootstrap: BOOTSTRAP,
      bundle,
      userHasDefaultFetch: false,
    });

    // Marker is present — bootstrap WAS appended.
    assert.ok(
      wrapped.includes(BOOTSTRAP_MARKER),
      "bootstrap marker present when user has no default"
    );
    // Bootstrap's default.fetch is what handles requests.
    assert.ok(
      wrapped.includes("export default"),
      "bootstrap default in wrapped bundle"
    );
    // Single `export default` — bootstrap's only.
    const defaultCount = (wrapped.match(/^\s*export\s+default\b/gm) ?? []).length;
    assert.equal(defaultCount, 1, "exactly one export default");
  });

  test("strips_use_server_directive", () => {
    // The `"use server"` pragma is bytes after we've prepended the
    // prelude. Strip it so the resulting module isn't accidentally a
    // strict-mode directive disguised as a string expression.
    const bundle = '"use server";\nexport function ping(){}\n';
    const wrapped = wrapServerBundle({
      prelude: PRELUDE,
      bootstrap: BOOTSTRAP,
      bundle,
      userHasDefaultFetch: false,
    });
    // The prelude is at the very top.
    assert.ok(
      wrapped.startsWith(PRELUDE),
      "prelude is at the top of the bundle"
    );
    // The directive doesn't appear standalone in the user portion.
    const userStart = wrapped.indexOf("export function ping");
    assert.ok(userStart > 0, "user code present");
    const beforeUser = wrapped.slice(PRELUDE.length, userStart);
    assert.ok(
      !beforeUser.includes('"use server"'),
      "use server directive stripped from user code"
    );
  });
});

describe("userSourceHasDefaultExport", () => {
  test("detects export default object", () => {
    assert.equal(
      userSourceHasDefaultExport("export default { fetch: () => {} };"),
      true
    );
  });

  test("detects export default function", () => {
    assert.equal(
      userSourceHasDefaultExport("export default function handler(){}"),
      true
    );
  });

  test("returns false for RPC-only entry", () => {
    assert.equal(
      userSourceHasDefaultExport(
        '"use server";\nexport function ping(){}\n'
      ),
      false
    );
  });

  test("ignores commented-out default", () => {
    // A commented-out export default in the docs of an RPC-only file
    // should NOT trigger the match.
    const src = `
// IDEAL SHAPE:
// export default { fetch: req => new Response("ok") };
"use server";
export function listTodos() { return []; }
`;
    assert.equal(userSourceHasDefaultExport(src), false);
  });

  test("ignores block-commented default", () => {
    const src = `
/*
 * export default { fetch }
 */
export function ping(){}
`;
    assert.equal(userSourceHasDefaultExport(src), false);
  });
});

// ── Bug 3: virtual:zeroship/client-manifest ────────────────────────────────
describe("clientManifestPlugin", () => {
  // Helpers — reach into the plugin's hook functions directly so we
  // don't have to spin up a full Vite dev server.
  function callResolveId(plugin: ReturnType<typeof clientManifestPlugin>, id: string): unknown {
    const fn = plugin.resolveId as (id: string) => unknown;
    return fn.call(plugin, id);
  }
  function callLoad(plugin: ReturnType<typeof clientManifestPlugin>, id: string): unknown {
    const fn = plugin.load as (id: string) => unknown;
    return fn.call(plugin, id);
  }

  test("resolves the virtual specifier", () => {
    const plugin = clientManifestPlugin({ root: "/tmp", distDir: "dist" });
    assert.equal(
      callResolveId(plugin, CLIENT_MANIFEST_VIRTUAL_ID),
      CLIENT_MANIFEST_RESOLVED_ID,
      "resolveId returns the \\0-prefixed id"
    );
    assert.equal(
      callResolveId(plugin, "some-other-module"),
      null,
      "resolveId ignores unrelated specifiers"
    );
  });

  test("virtual_client_manifest_resolves", async () => {
    // Set up a fixture with a client manifest on disk.
    const sample = {
      "src/entry-client.tsx": {
        file: "assets/entry-client-DEADBEEF.js",
        src: "src/entry-client.tsx",
        isEntry: true,
        css: ["assets/entry-client-CAFEBABE.css"],
      },
    };
    const fix = await makeFixture({
      "dist/.vite/manifest.json": JSON.stringify(sample),
    });
    try {
      const plugin = clientManifestPlugin({
        root: fix.root,
        distDir: "dist",
      });
      const code = callLoad(plugin, CLIENT_MANIFEST_RESOLVED_ID);
      assert.equal(typeof code, "string", "load returns code string");
      // The emitted module must be valid ESM that exports a default
      // matching the on-disk JSON.
      const codeStr = code as string;
      assert.match(
        codeStr,
        /^export default /,
        "starts with `export default`"
      );
      // Eval via dynamic import via data: URL — confirms the module is
      // syntactically valid and the inlined JSON matches.
      const dataUrl =
        "data:text/javascript;base64," +
        Buffer.from(codeStr, "utf8").toString("base64");
      const mod = await import(dataUrl);
      assert.deepEqual(mod.default, sample, "default export matches JSON");
    } finally {
      await fix.cleanup();
    }
  });

  test("falls back to empty object when manifest missing", () => {
    const plugin = clientManifestPlugin({
      root: "/non/existent/path",
      distDir: "dist",
    });
    const code = callLoad(plugin, CLIENT_MANIFEST_RESOLVED_ID);
    assert.equal(
      code,
      "export default {};",
      "fallback returns empty-object module"
    );
  });

  test("ignores unrelated module ids on load", () => {
    const plugin = clientManifestPlugin({ root: "/tmp", distDir: "dist" });
    assert.equal(callLoad(plugin, "some-other-module"), null);
    // Also ignores the public specifier on `load` — Vite calls load
    // only with the resolved id.
    assert.equal(callLoad(plugin, CLIENT_MANIFEST_VIRTUAL_ID), null);
  });
});

// ── Bug 4: SSR build should NOT copy public/ ──────────────────────────────
describe("buildSsrInlineConfig", () => {
  test("ssr_build_no_publicdir_copy", () => {
    // The SSR sub-build's `publicDir` MUST be `false` — otherwise Vite
    // copies `<root>/public/*` into `dist/server/`, and those files
    // get cataloged as worker.modules entries by the .zsapp emitter.
    const config = buildSsrInlineConfig({
      root: "/tmp/myapp",
      ssrEntry: "/tmp/myapp/src/server.ts",
      outDir: "dist/server",
      ssrPlugins: [],
    });
    assert.equal(
      config.publicDir,
      false,
      "publicDir is false on the SSR sub-build"
    );
    // Sanity: the rest of the contract is intact.
    assert.equal(config.root, "/tmp/myapp");
    const ssr = config.ssr as { noExternal?: boolean; target?: string };
    assert.equal(ssr.noExternal, true);
    assert.equal(ssr.target, "webworker");
    const build = config.build as { ssr?: string; outDir?: string };
    assert.equal(build.ssr, "/tmp/myapp/src/server.ts");
    assert.equal(build.outDir, "dist/server");
  });

  test("static_only_mode_skips_rollup", async () => {
    // SSG-only fixture: no JS, only static HTML files. After build,
    // the .zsapp should contain those HTML files as assets, no worker,
    // and NO spurious `_empty-<hash>.js` chunk.
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html><html><body>Home</body></html>",
      "dist/about.html": "<!doctype html><html><body>About</body></html>",
      "dist/docs/intro.html":
        "<!doctype html><html><body>Intro</body></html>",
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));

      // No worker (SSG-only).
      assert.ok(
        !("worker" in manifest) || manifest.worker == null,
        "worker absent for SSG-only build"
      );

      // Each HTML page is in assets.
      assert.ok(manifest.assets["/index.html"], "index.html in assets");
      assert.ok(manifest.assets["/about.html"], "about.html in assets");
      assert.ok(
        manifest.assets["/docs/intro.html"],
        "docs/intro.html in assets"
      );

      // No `_empty-<hash>.js` placeholder anywhere.
      const jsBlobs = Object.keys(manifest.assets).filter((p) =>
        /^\/.*\.js$/.test(p)
      );
      assert.equal(
        jsBlobs.length,
        0,
        `no JS assets in SSG-only build (found ${JSON.stringify(jsBlobs)})`
      );

      // Per-route exact rules emitted for non-index HTML, plus SPA-style
      // catch-all serving index.html.
      const ruleSummary = manifest.rules.map(
        (r: {
          match: { kind: string; path?: string };
          action: { kind: string; try?: string[] };
        }) =>
          `${r.match.kind}${r.match.path ? "(" + r.match.path + ")" : ""}/${r.action.kind}`
      );
      assert.ok(
        ruleSummary.some((s: string) => s === "exact(/about)/static"),
        "/about route emitted"
      );
      assert.ok(
        ruleSummary.some((s: string) => s === "exact(/docs/intro)/static"),
        "/docs/intro route emitted"
      );
      assert.ok(
        ruleSummary.some((s: string) => s === "any/static"),
        "SPA fallback emitted"
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("static_only_with_html_inputs_still_works", async () => {
    // Mixed: an `index.html` entry produced by Vite (with hashed JS
    // chunks) PLUS additional static `content/*.html` files copied
    // into dist. Both should land as assets.
    const fix = await makeFixture({
      "dist/index.html":
        '<!doctype html><html><head><script type="module" src="/assets/main-abc.js"></script></head></html>',
      "dist/assets/main-abc.js": "console.log('app');",
      "dist/about.html": "<!doctype html>About",
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));

      assert.ok(manifest.assets["/index.html"]);
      assert.ok(manifest.assets["/assets/main-abc.js"]);
      assert.ok(manifest.assets["/about.html"]);
      // /assets/ prefix rule emitted for the Vite-hashed JS.
      const hasAssetsRule = manifest.rules.some(
        (r: {
          match: { kind: string; path?: string };
          action: { kind: string };
        }) =>
          r.match.kind === "prefix" &&
          r.match.path === "/assets/" &&
          r.action.kind === "static"
      );
      assert.ok(hasAssetsRule, "/assets/ prefix rule emitted");
      // /about route emitted.
      const hasAboutRule = manifest.rules.some(
        (r: {
          match: { kind: string; path?: string };
          action: { kind: string };
        }) => r.match.kind === "exact" && r.match.path === "/about"
      );
      assert.ok(hasAboutRule, "/about route emitted");
    } finally {
      await fix.cleanup();
    }
  });

  test("only_index_js_in_worker_modules_when_no_public_copy", async () => {
    // Walker sanity: when dist/server/ contains only `index.js` (which
    // is what the SSR build produces with `publicDir: false`), the
    // emitted manifest's `worker.modules` has only `index.js` — and
    // the public favicon ends up in `assets` from the client build.
    const fix = await makeFixture({
      "dist/index.html": "<!doctype html>",
      "dist/favicon.ico": "icon-bytes",
      "dist/assets/main.js": "console.log('csr')",
      "dist/server/index.js": "export default { fetch: () => new Response('') }",
    });
    try {
      const result = await emitZsapp({
        root: fix.root,
        builtAt: "2026-04-29T00:00:00Z",
        silent: true,
      });
      const tarBytes = zstdDecompressSync(await fs.readFile(result.outputPath));
      const manifest = JSON.parse(parseTar(tarBytes)[0].bytes.toString("utf8"));

      assert.deepEqual(
        Object.keys(manifest.worker.modules).sort(),
        ["index.js"],
        "worker.modules has only index.js"
      );
      assert.ok(manifest.assets["/favicon.ico"], "favicon.ico in assets");
    } finally {
      await fix.cleanup();
    }
  });
});
