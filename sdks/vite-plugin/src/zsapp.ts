// sdks/vite-plugin/src/zsapp.ts
//
// Emit `.zsapp` artifacts from the `dist/` directory produced by Vite.
//
// The build pipeline:
//
//   src/server/index.ts  ──► vite build --ssr ──► dist/server/index.js
//   src/client/main.tsx  ──► vite build       ──► dist/<assets...>
//                                                 dist/index.html
//
// We then walk `dist/`, content-hash every file, emit a manifest, and pack
// it into a tar.zst archive at `dist/app.zsapp`. The wire format is
// defined in `docs/reference/zsapp.md` (schema v2).
//
// The control plane ingests this via `POST /api/apps/{id}/deploy`.

import { createHash } from "node:crypto";
import { promises as fs } from "node:fs";
import { dirname, join, posix, relative, resolve, sep } from "node:path";
import { zstdCompressSync } from "node:zlib";
import { create as tarCreate } from "tar";
import mime from "mime";

// ── Types matching crates/core/src/types.rs ────────────────────────────────

/** SHA-256 hex (lowercase, 64 chars). */
type Sha256Hex = string;

interface CacheCtl {
  max_age: number;
  swr_window?: number;
  immutable?: boolean;
  background_refresh?: boolean;
  stale_on_error?: boolean;
}

interface AssetEntry {
  hash: Sha256Hex;
  content_type: string;
  size: number;
  cache?: CacheCtl;
  /** Set on assets.put. Build-time emissions leave this at 0. */
  updated_at?: number;
}

type HttpMethod =
  | "GET"
  | "POST"
  | "PUT"
  | "PATCH"
  | "DELETE"
  | "HEAD"
  | "OPTIONS"
  | "*";

type WorkerMode = "rpc" | "ssr";

type Match =
  | { kind: "exact"; method?: HttpMethod; path: string }
  | { kind: "prefix"; method?: HttpMethod; path: string }
  | { kind: "glob"; method?: HttpMethod; path: string }
  | { kind: "any" };

type Action =
  | {
      kind: "static";
      try: string[];
      cache?: CacheCtl;
      status?: number;
    }
  | {
      kind: "worker";
      mode: WorkerMode;
      cache?: CacheCtl;
      rate_limit?: { rpm?: number; rps?: number; per?: "ip" | "session" | "app" };
    }
  | { kind: "redirect"; to: string; status: number }
  | { kind: "rewrite"; to: string };

interface Rule {
  match: Match;
  action: Action;
}

interface WorkerCode {
  entry: string;
  modules: Record<string, Sha256Hex>;
}

interface ManifestMetadata {
  compiler?: string;
  built_at: string;
}

interface Manifest {
  version: 2;
  worker?: WorkerCode | null;
  rules: Rule[];
  assets: Record<string, AssetEntry>;
  runtime_assets: Record<string, AssetEntry>;
  asset_version: 0;
  sourcemaps: Record<Sha256Hex, Sha256Hex>;
  metadata: ManifestMetadata;
}

// ── Public configuration ───────────────────────────────────────────────────

export interface ZsappOptions {
  /** Project root (defaults to Vite's resolved root). */
  root: string;
  /** `outDir` of the client/static build. Default: `dist`. */
  distDir?: string;
  /** Subdir under `distDir` containing the worker bundle. Default: `server`. */
  serverDir?: string;
  /** Output archive path. Default: `<distDir>/app.zsapp`. */
  outputPath?: string;
  /** The compiler identifier baked into `metadata.compiler`. */
  compiler?: string;
  /** Override `built_at` (RFC 3339). Useful for reproducible builds in tests. */
  builtAt?: string;
  /** Path to the asset prefix Vite emits ("/assets/" by default). */
  assetPrefix?: string;
  /** Quiet mode — suppress info logs. Default: false. */
  silent?: boolean;
}

export interface ZsappResult {
  /** Absolute path of the emitted `.zsapp` archive. */
  outputPath: string;
  /** The manifest that was packed (with deploy_hash absent — control plane fills it). */
  manifest: Manifest;
  /** Total compressed bytes. */
  archiveSize: number;
  /** Number of distinct content blobs in the archive (manifest is not counted). */
  blobCount: number;
}

// ── Implementation ─────────────────────────────────────────────────────────

const DEFAULT_DIST_DIR = "dist";
const DEFAULT_SERVER_SUBDIR = "server";
const DEFAULT_OUTPUT_NAME = "app.zsapp";
const STAGING_DIR_NAME = ".zsapp";
const DEFAULT_ASSET_PREFIX = "/assets/";

/**
 * Build a `.zsapp` archive from the project's `dist/` directory.
 *
 * Walks `dist/` (excluding the staging dir), hashes every file, builds the
 * manifest, packs into `tar.zst`. The resulting archive is the exact bytes
 * the CLI uploads to the control plane.
 */
export async function emitZsapp(
  options: ZsappOptions
): Promise<ZsappResult> {
  const root = resolve(options.root);
  const distDir = resolve(root, options.distDir ?? DEFAULT_DIST_DIR);
  const serverSubdir = options.serverDir ?? DEFAULT_SERVER_SUBDIR;
  const serverDir = resolve(distDir, serverSubdir);
  const outputPath = resolve(
    options.outputPath ?? resolve(distDir, DEFAULT_OUTPUT_NAME)
  );
  const stagingDir = resolve(distDir, STAGING_DIR_NAME);
  const assetPrefix = options.assetPrefix ?? DEFAULT_ASSET_PREFIX;
  const compiler = options.compiler ?? "@zeroship/vite-plugin";
  const builtAt = options.builtAt ?? new Date().toISOString();
  const log = options.silent
    ? () => {}
    : (msg: string) => console.log(`[zeroship:zsapp] ${msg}`);

  if (!(await pathExists(distDir))) {
    throw new Error(`zsapp: dist dir not found at ${distDir}`);
  }

  // 1. Walk dist/ — collect candidate files with their absolute path,
  //    serving URL path (relative to dist root, '/'-prefixed, posix slashes),
  //    and tag whether they belong to the worker bundle.
  const items = await collectFiles(distDir, {
    distDir,
    serverDir,
    stagingDir,
    outputPath,
  });

  if (items.length === 0) {
    throw new Error(
      `zsapp: no files found under ${distDir} — did the build run?`
    );
  }

  // 2. Hash every file. Hashing happens once and the result drives
  //    every downstream reference (manifest entries, blob filenames,
  //    cross-validation).
  const hashed = await Promise.all(
    items.map(async (item) => {
      const bytes = await fs.readFile(item.absPath);
      const hash = sha256Hex(bytes);
      return { ...item, hash, size: bytes.length, bytes };
    })
  );

  // Dedupe blobs by hash. Two identical files become one tar entry.
  const blobsByHash = new Map<Sha256Hex, Buffer>();
  for (const f of hashed) {
    if (!blobsByHash.has(f.hash)) blobsByHash.set(f.hash, f.bytes);
  }

  // 3. Partition into worker vs. asset vs. sourcemap.
  const workerFiles = hashed.filter((f) => f.kind === "worker");
  const assetFiles = hashed.filter((f) => f.kind === "asset");
  const sourcemapFiles = hashed.filter((f) => f.kind === "sourcemap");

  // 4. Build manifest.assets (only for non-worker, non-sourcemap files).
  const assets: Record<string, AssetEntry> = {};
  for (const f of assetFiles) {
    assets[f.urlPath] = {
      hash: f.hash,
      content_type: detectContentType(f.relPath),
      size: f.size,
    };
  }

  // 5. Build manifest.sourcemaps — assetHash → sourcemapHash.
  //    Match a `foo.js.map` to its sibling `foo.js`. Vite emits sourcemaps
  //    next to the asset, so the lookup is "drop .map suffix".
  const sourcemaps: Record<Sha256Hex, Sha256Hex> = {};
  for (const sm of sourcemapFiles) {
    // sm.relPath like "assets/index-abc.js.map"
    const targetRel = sm.relPath.replace(/\.map$/, "");
    const target = assetFiles.find((a) => a.relPath === targetRel)
      ?? workerFiles.find((w) => w.relPath === targetRel);
    if (!target) {
      // Orphan sourcemap — skip silently. Vite occasionally emits
      // `__commonjsHelpers.js.map` when there's no corresponding output.
      continue;
    }
    sourcemaps[target.hash] = sm.hash;
  }

  // 6. Build manifest.worker.
  let worker: WorkerCode | null = null;
  if (workerFiles.length > 0) {
    // Pick the entry. By convention Vite's SSR build emits `index.js` in
    // `dist/server/`. If the user customizes the entry filename, we still
    // pick it deterministically by checking for the conventional names
    // first, then falling back to the alphabetically-first .js file.
    const entry = pickWorkerEntry(workerFiles);
    const modules: Record<string, Sha256Hex> = {};
    for (const f of workerFiles) {
      // Specifier is the path relative to the server dir (e.g. "index.js").
      const specifier = posixRelative(serverDir, f.absPath);
      modules[specifier] = f.hash;
    }
    worker = { entry, modules };
  }

  // 7. Build manifest.rules — see (e) in the design.
  const rules = buildRules({
    assetPrefix,
    assets,
    hasWorker: worker != null,
  });

  // 8. Assemble the manifest. Rust's serde accepts both `null` and
  //    omitted-field for `Option<WorkerCode>`, but we omit the field
  //    entirely when there's no worker — this matches the spec's shape
  //    notes ("null/missing means SSG-only") and keeps the canonical
  //    JSON shorter.
  const manifest: Manifest = {
    version: 2,
    rules,
    assets,
    runtime_assets: {},
    asset_version: 0,
    sourcemaps,
    metadata: { compiler, built_at: builtAt },
  };
  if (worker != null) {
    manifest.worker = worker;
  }

  // 9. Validate cross-references. Catches bugs where a manifest hash
  //    doesn't have a matching tar entry (which would 400 on the server).
  validateManifest(manifest, blobsByHash);

  // 10. Stream the archive.
  await fs.rm(stagingDir, { recursive: true, force: true });
  await fs.mkdir(stagingDir, { recursive: true });

  // tar.create needs a `cwd` and a list of file paths relative to it.
  // We materialize manifest.json + blobs/<hash> into the staging dir and
  // then pass them in deterministic order (manifest first, blobs sorted
  // by hash).
  const manifestJson = canonicalJson(manifest);
  await fs.writeFile(join(stagingDir, "manifest.json"), manifestJson);

  const blobsDir = join(stagingDir, "blobs");
  await fs.mkdir(blobsDir, { recursive: true });
  const sortedHashes = [...blobsByHash.keys()].sort();
  await Promise.all(
    sortedHashes.map((h) =>
      fs.writeFile(join(blobsDir, h), blobsByHash.get(h)!)
    )
  );

  const tarPath = join(stagingDir, "archive.tar");

  // tar.create — entries are written in argument order. Manifest first
  // (REQUIRED by the spec), then blobs in hash order (deterministic).
  await tarCreate(
    {
      file: tarPath,
      cwd: stagingDir,
      portable: true,
      // Don't emit gzip; we zstd-compress the whole tar.
      gzip: false,
    },
    [
      "manifest.json",
      ...sortedHashes.map((h) => `blobs/${h}`),
    ]
  );

  const tarBytes = await fs.readFile(tarPath);
  const compressed = zstdCompressSync(tarBytes);

  await fs.mkdir(dirname(outputPath), { recursive: true });
  await fs.writeFile(outputPath, compressed);

  // Clean up staging dir — the archive is the artifact. Keep it
  // around in --debug mode would be nice, but quiet by default.
  await fs.rm(stagingDir, { recursive: true, force: true });

  log(
    `wrote ${relative(root, outputPath)} ` +
      `(${formatBytes(compressed.length)}, ${blobsByHash.size} blobs, ` +
      `${hashed.length} files, ` +
      `${assetFiles.length} assets, ` +
      `${workerFiles.length} worker modules` +
      `${sourcemapFiles.length > 0 ? `, ${sourcemapFiles.length} sourcemaps` : ""})`
  );

  return {
    outputPath,
    manifest,
    archiveSize: compressed.length,
    blobCount: blobsByHash.size,
  };
}

// ── File-walking ────────────────────────────────────────────────────────────

interface CollectedFile {
  absPath: string;
  /** Path relative to `distDir` with posix slashes (e.g., "assets/foo.js"). */
  relPath: string;
  /** URL path the asset is served at, with leading `/`. */
  urlPath: string;
  /** Categorization. */
  kind: "worker" | "asset" | "sourcemap";
}

/** Recursively walk `distDir`, returning files split into worker / asset / sourcemap. */
async function collectFiles(
  startDir: string,
  ctx: {
    distDir: string;
    serverDir: string;
    stagingDir: string;
    outputPath: string;
  }
): Promise<CollectedFile[]> {
  const out: CollectedFile[] = [];

  async function walk(dir: string): Promise<void> {
    const entries = await fs.readdir(dir, { withFileTypes: true });
    for (const e of entries) {
      const abs = join(dir, e.name);

      // Skip the staging dir to prevent pre-existing archives from
      // self-ingesting.
      if (abs === ctx.stagingDir) continue;
      // Skip the output archive itself if it lives under distDir.
      if (abs === ctx.outputPath) continue;
      // Skip Vite's own metadata. .vite/manifest.json is for the
      // build pipeline, not for the deploy artifact.
      if (e.name === ".vite") continue;

      if (e.isDirectory()) {
        await walk(abs);
        continue;
      }
      if (!e.isFile()) continue;

      // Categorize. Sourcemap check first — a `.map` file that lives under
      // dist/server/ pairs with the worker module via the manifest's
      // `sourcemaps` map, NOT as a worker module itself. Same for client-side
      // assets: `foo.js.map` is metadata for `foo.js`, not an asset of its
      // own.
      const isSourcemap = e.name.endsWith(".map");
      const inWorker =
        abs === ctx.serverDir ||
        abs.startsWith(ctx.serverDir + sep);

      const relFromDist = posixRelative(ctx.distDir, abs);

      let kind: CollectedFile["kind"];
      if (isSourcemap) {
        kind = "sourcemap";
      } else if (inWorker) {
        kind = "worker";
      } else {
        kind = "asset";
      }
      // Worker URL is irrelevant — workers aren't served at URL paths.
      // We still set urlPath for symmetry; nothing reads it for non-asset
      // kinds.
      const urlPath = "/" + relFromDist;

      out.push({
        absPath: abs,
        relPath: relFromDist,
        urlPath,
        kind,
      });
    }
  }

  await walk(startDir);
  return out;
}

// ── Manifest helpers ────────────────────────────────────────────────────────

function pickWorkerEntry(workerFiles: CollectedFile[]): string {
  // Prefer conventional names emitted by Vite SSR builds.
  const candidates = ["index.js", "index.mjs", "main.js", "main.mjs", "server.js"];
  // Strip the server-dir prefix so specifiers are relative to the worker bundle.
  const specifiers = workerFiles.map((f) => {
    // f.relPath is like "server/index.js" — strip the first segment.
    const i = f.relPath.indexOf("/");
    return i === -1 ? f.relPath : f.relPath.slice(i + 1);
  });

  for (const c of candidates) {
    if (specifiers.includes(c)) return c;
  }
  // Fall back to alphabetically-first .js/.mjs file. Deterministic.
  const js = specifiers.filter((s) => /\.(js|mjs|cjs)$/.test(s)).sort();
  if (js.length > 0) return js[0];
  // Last resort: first file.
  specifiers.sort();
  return specifiers[0];
}

function buildRules(opts: {
  assetPrefix: string;
  assets: Record<string, AssetEntry>;
  hasWorker: boolean;
}): Rule[] {
  const rules: Rule[] = [];
  const { assetPrefix, assets, hasWorker } = opts;

  // Asset prefix → static, immutable cache (1y).
  if (Object.keys(assets).some((p) => p.startsWith(assetPrefix))) {
    rules.push({
      match: { kind: "prefix", path: assetPrefix },
      action: {
        kind: "static",
        try: ["$path"],
        cache: {
          max_age: 31_536_000, // 1 year
          immutable: true,
        },
      },
    });
  }

  // Common public files (favicon.ico, robots.txt, sitemap.xml). Each
  // gets a per-path Match::Exact rule.
  for (const path of ["/favicon.ico", "/robots.txt", "/sitemap.xml"]) {
    if (assets[path]) {
      rules.push({
        match: { kind: "exact", path },
        action: { kind: "static", try: [path] },
      });
    }
  }

  // Prerendered HTML routes. Treat any `*.html` under dist/client/ as a
  // route candidate, with `/index.html` reserved as the SPA fallback.
  // Each HTML at `/foo.html` → Match::Exact /foo → Static{ try: ["/foo.html"] }.
  // /index.html is handled by the SPA-fallback rule (below) for
  // SSG-only builds; for SSR builds the worker handles routing.
  const htmlPaths = Object.keys(assets).filter(
    (p) => p.endsWith(".html") && p !== "/index.html"
  );
  for (const htmlPath of htmlPaths) {
    // /foo.html → /foo, /a/b.html → /a/b
    const route = htmlPath.replace(/\.html$/, "");
    rules.push({
      match: { kind: "exact", path: route },
      action: { kind: "static", try: [htmlPath] },
    });
  }

  if (hasWorker) {
    // RPC: POST /_rpc/* → Worker (rpc).
    rules.push({
      match: { kind: "prefix", method: "POST", path: "/_rpc/" },
      action: { kind: "worker", mode: "rpc" },
    });
    // Catch-all: any → Worker (ssr). The worker's default.fetch decides.
    rules.push({
      match: { kind: "any" },
      action: { kind: "worker", mode: "ssr" },
    });
  } else {
    // SSG-only: serve index.html as SPA fallback if it exists, else 404.
    if (assets["/index.html"]) {
      rules.push({
        match: { kind: "any" },
        action: {
          kind: "static",
          try: ["$path", "/index.html"],
        },
      });
    } else if (assets["/404.html"]) {
      rules.push({
        match: { kind: "any" },
        action: {
          kind: "static",
          try: ["/404.html"],
          status: 404,
        },
      });
    }
    // If neither exists, no catch-all rule — the gateway will 404 on no match.
  }

  return rules;
}

/** Throw if the manifest contains hash references that aren't in `blobsByHash`. */
function validateManifest(
  m: Manifest,
  blobsByHash: Map<Sha256Hex, Buffer>
): void {
  // Every asset hash must exist as a blob.
  for (const [path, entry] of Object.entries(m.assets)) {
    if (!blobsByHash.has(entry.hash)) {
      throw new Error(
        `zsapp: asset ${path} references hash ${entry.hash} but the blob is missing`
      );
    }
    if (!isSha256Hex(entry.hash)) {
      throw new Error(
        `zsapp: asset ${path} hash ${entry.hash} is not lowercase 64-char sha256 hex`
      );
    }
  }
  // Every sourcemap key/value must exist as a blob (and be sha256 hex).
  for (const [k, v] of Object.entries(m.sourcemaps)) {
    if (!isSha256Hex(k) || !isSha256Hex(v)) {
      throw new Error(
        `zsapp: sourcemaps entry ${k} -> ${v} contains non-sha256 hex`
      );
    }
    if (!blobsByHash.has(k)) {
      throw new Error(
        `zsapp: sourcemap key ${k} has no corresponding asset blob`
      );
    }
    if (!blobsByHash.has(v)) {
      throw new Error(
        `zsapp: sourcemap value ${v} has no corresponding blob`
      );
    }
  }
  // Worker checks.
  if (m.worker != null) {
    if (!m.worker.entry || !(m.worker.entry in m.worker.modules)) {
      throw new Error(
        `zsapp: worker.entry ${JSON.stringify(m.worker.entry)} is not a key in worker.modules`
      );
    }
    for (const [spec, hash] of Object.entries(m.worker.modules)) {
      if (!isSha256Hex(hash)) {
        throw new Error(
          `zsapp: worker.modules[${spec}] hash ${hash} is not lowercase 64-char sha256 hex`
        );
      }
      if (!blobsByHash.has(hash)) {
        throw new Error(
          `zsapp: worker.modules[${spec}] hash ${hash} has no corresponding blob`
        );
      }
    }
  }
  // Every rule's `try` chain (with no captures or $path) must point at
  // a path that's a key in `assets`. We allow `$path` (resolved at
  // request time) and any path containing `[name]` (glob captures).
  for (let i = 0; i < m.rules.length; i++) {
    const rule = m.rules[i];
    if (rule.action.kind !== "static") continue;
    for (const t of rule.action.try) {
      if (t === "$path" || t.includes("[")) continue;
      if (!(t in m.assets)) {
        throw new Error(
          `zsapp: rule ${i}: try chain references ${t} but it's not in assets`
        );
      }
    }
  }
}

// ── Misc helpers ────────────────────────────────────────────────────────────

function sha256Hex(bytes: Buffer): Sha256Hex {
  return createHash("sha256").update(bytes).digest("hex");
}

function isSha256Hex(s: string): boolean {
  return /^[0-9a-f]{64}$/.test(s);
}

function detectContentType(relPath: string): string {
  // mime@3 returns null for unknown extensions; fall back to the
  // generic byte stream type.
  return mime.getType(relPath) ?? "application/octet-stream";
}

/** Path relative to a base, normalized to posix slashes (no leading '/'). */
function posixRelative(from: string, to: string): string {
  const r = relative(from, to);
  return r.split(sep).join(posix.sep);
}

/** Lexicographically-keyed canonical JSON. Stable across runs. */
function canonicalJson(value: unknown): string {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value !== null && typeof value === "object") {
    const sorted: Record<string, unknown> = {};
    for (const k of Object.keys(value as Record<string, unknown>).sort()) {
      sorted[k] = sortKeys((value as Record<string, unknown>)[k]);
    }
    return sorted;
  }
  return value;
}

async function pathExists(p: string): Promise<boolean> {
  try {
    await fs.stat(p);
    return true;
  } catch {
    return false;
  }
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
  return `${(n / (1024 * 1024)).toFixed(2)}MB`;
}
