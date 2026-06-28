// sdks/vite-plugin/src/zship.ts
//
// Emit `.zship` artifacts from the `dist/` directory produced by Vite.
//
// The build pipeline:
//
//   src/server/index.ts  ──► vite build --ssr ──► dist/server/index.js
//   src/client/main.tsx  ──► vite build       ──► dist/<assets...>
//                                                 dist/index.html
//
// We then walk `dist/`, content-hash every file, emit a manifest, and pack
// it into a tar.zst archive at `dist/app.zship`. The wire format is
// defined in `docs/reference/zship.md` (schema v2).
//
// The control plane ingests this via `POST /api/apps/{id}/deploy`.

import { createHash } from "node:crypto";
import { promises as fs } from "node:fs";
import { dirname, join, posix, relative, resolve, sep } from "node:path";
import {
  brotliCompress,
  constants as zlibConstants,
  gzipSync,
  zstdCompressSync,
} from "node:zlib";
import { promisify } from "node:util";
import { create as tarCreate } from "tar";
import mime from "mime";

import {
  discoverMigrations,
  GEN_TYPES_OUT_DEFAULT,
  RUNTIME_DESCRIPTOR_FILE,
} from "./migrations.js";

const brotliCompressAsync = promisify(brotliCompress);

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

interface AssetVariant {
  /** SHA-256 hex of the COMPRESSED bytes. */
  hash: Sha256Hex;
  /** Compressed byte count. */
  size: number;
}

interface AssetEntry {
  hash: Sha256Hex;
  content_type: string;
  size: number;
  cache?: CacheCtl;
  /** Set on assets.put. Build-time emissions leave this at 0. */
  updated_at?: number;
  /**
   * Pre-compressed encoding variants, keyed by HTTP `Content-Encoding`
   * token. v1: only `"br"` and `"gzip"` are accepted by the validator.
   * Empty / omitted when no variants were emitted (e.g. the asset is
   * already binary, or already-compressed).
   */
  variants?: Record<string, AssetVariant>;
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
  /** Manifest schema version. v1 is the initial published shape. */
  version: 1;
  worker?: WorkerCode | null;
  assets: Record<string, AssetEntry>;
  runtime_assets: Record<string, AssetEntry>;
  asset_version: 0;
  sourcemaps: Record<Sha256Hex, Sha256Hex>;
  metadata: ManifestMetadata;
  /** Unified resource tree — see `docs/proposals/rpc.md` §7. */
  resources?: Record<string, Record<string, unknown>>;
  /** Wire transformer: `"json"` (default) or `"superjson"`. */
  transformer?: "superjson" | "json";
  /** Inert outbound TCP request hints. Grants live only in control's table. */
  net?: NetConfig;
  /**
   * The op.* DSL migrations carried by the bundle (PR4 A4). Each entry's `name`
   * is the committed `<14-digit>_<desc>.ir.json` filename; `hash` is the sha256 of
   * the committed bytes (consumed VERBATIM by the packer — never re-emitted). The
   * control plane hands these to `zeroship-migrate` before go-live (§5.1).
   */
  migrations?: MigrationFileEntry[];
  /**
   * The generated runtime schema descriptor (`schema.runtime.json`) carried by
   * the bundle (migration-first P4a). Content-addressed like a migration:
   * `{ hash }` is the sha256 of the `gen-types`-emitted descriptor bytes,
   * staged as a blob. `undefined` when the app ships no migrations / no
   * descriptor. Mirrors the Rust `bundle::manifest::RuntimeDescriptorEntry`.
   *
   * The runtime/worker path reads this descriptor and exposes it as
   * `globalThis.__zsRuntimeDescriptor` for bootstrap schema install.
   */
  runtime_descriptor?: RuntimeDescriptorEntry;
}

interface NetConfig {
  requests?: NetRequest[];
}

interface NetRequest {
  host: string;
  port: number;
  reason: string;
}

/** One migration file carried by the `.zship` (`manifest.migrations[i]`). Mirrors
 *  the Rust `bundle::manifest::MigrationFileEntry`. */
interface MigrationFileEntry {
  /** The committed `.ir.json` filename (bare; no path separators). */
  name: string;
  /** sha256 (lowercase, 64 hex) of the committed `.ir.json` blob. */
  hash: Sha256Hex;
}

/** The runtime schema descriptor carried by the `.zship`
 *  (`manifest.runtime_descriptor`). Mirrors the Rust
 *  `bundle::manifest::RuntimeDescriptorEntry`. */
interface RuntimeDescriptorEntry {
  /** sha256 (lowercase, 64 hex) of the `schema.runtime.json` blob. */
  hash: Sha256Hex;
}

// ── Public configuration ───────────────────────────────────────────────────

/**
 * Pre-compression toggles. Default: brotli on, gzip off.
 *
 * Brotli at quality 11 is slow (1–3 sec per MB) but the payoff is
 * 15–25% smaller bytes on the wire vs. gzip at level 9. Gzip is kept
 * around for ancient HTTP intermediaries that still don't speak `br`.
 *
 * Skip emission entirely on assets where compression doesn't reduce
 * size (already-compressed binaries, fonts, small text).
 */
export interface PrecompressOptions {
  brotli?: boolean;
  gzip?: boolean;
}

export interface ZshipOptions {
  /** Project root (defaults to Vite's resolved root). */
  root: string;
  /** `outDir` of the client/static build. Default: `dist`. */
  distDir?: string;
  /** Subdir under `distDir` containing the worker bundle. Default: `server`. */
  serverDir?: string;
  /** Output archive path. Default: `<distDir>/app.zship`. */
  outputPath?: string;
  /** The compiler identifier baked into `metadata.compiler`. */
  compiler?: string;
  /** Override `built_at` (RFC 3339). Useful for reproducible builds in tests. */
  builtAt?: string;
  /** Path to the asset prefix Vite emits ("/assets/" by default). */
  assetPrefix?: string;
  /** Quiet mode — suppress info logs. Default: false. */
  silent?: boolean;
  /**
   * Pre-compress text-like assets and emit `variants` entries the
   * gateway can serve via `Accept-Encoding` negotiation.
   *
   * Default: `{ brotli: true, gzip: false }`. Brotli alone covers
   * every browser made in the last decade; gzip is opt-in for legacy.
   */
  precompress?: PrecompressOptions;
  /**
   * Whether the user's SSR entry exports its own `default.fetch`.
   *
   * Drives the catch-all resource choice:
   *   - true  → `/[...rest]` resource without a routing action →
   *             gateway forwards to the worker as SSR (user owns routing)
   *   - false → `/[...rest]` static action serving `["$path", "/index.html"]`
   *             (RPC-only app; SPA shell handles unmatched URLs)
   *
   * Default: `true` — conservative; an unwanted SSR catch-all 404s,
   * which is preferable to a stale shell on an intended SSR route.
   * The vite-plugin sets this from a regex probe of the SSR entry
   * source before Rollup runs (see `build.ts`).
   */
  userHasDefaultFetch?: boolean;
  /**
   * Extra manifest fields (`resources`, `transformer`) computed by
   * `src/manifest.ts`. Merged into the auto-derived URL resources;
   * `rpcExtras.resources` wins on key collisions.
   *
   * Schemas (Zod) live on the procedures at runtime; the synthetic
   * SSR entry validates with them. The manifest never carries
   * JSONSchemas.
   */
  rpcExtras?: {
    resources: Record<string, Record<string, unknown>>;
    transformer: "superjson" | "json";
    net?: NetConfig;
  };
  /**
   * The op.* DSL migrations dir relative to `root` (default `migrations`). The
   * packer discovers `migrations/*.ts`, records each one lacking a committed
   * `<name>.ir.json` via the PR4a recorder (the vite-plugin is a thin client of the
   * SAME recorder — it shells the `zeroship-migrate-js` CLI; never an in-process
   * eval of untrusted `.ts`), reads each committed `.ir.json` VERBATIM, and
   * contributes `{ name, hash }` entries into `manifest.migrations` + stages the
   * blob. Set `migrations: false` to disable discovery.
   */
  migrations?:
    | false
    | {
        /** Migrations dir relative to root (default `migrations`). */
        dir?: string;
        /** The declaring/deploying app (`app_…`). */
        ownerApp?: string;
        /** The hosted recorder URL (falls back to `ZEROSHIP_RECORDER_URL`). */
        recorderUrl?: string;
        /** Path to the `zeroship-migrate-js` CLI (default on PATH). */
        cliPath?: string;
        /**
         * The `gen-types` output dir relative to `root` (default
         * `GEN_TYPES_OUT_DEFAULT`). The packer reads
         * `<genTypesOut>/schema.runtime.json` (when present) and carries it as
         * the manifest's content-addressed `runtime_descriptor` blob (P4a).
         */
        genTypesOut?: string;
      };
}

export interface ZshipResult {
  /** Absolute path of the emitted `.zship` archive. */
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
const DEFAULT_OUTPUT_NAME = "app.zship";
const STAGING_DIR_NAME = ".zship";
const DEFAULT_ASSET_PREFIX = "/assets/";

/**
 * Build a `.zship` archive from the project's `dist/` directory.
 *
 * Walks `dist/` (excluding the staging dir), hashes every file, builds the
 * manifest, packs into `tar.zst`. The resulting archive is the exact bytes
 * the CLI uploads to the control plane.
 */
export async function emitZship(
  options: ZshipOptions
): Promise<ZshipResult> {
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
  const precompress: Required<PrecompressOptions> = {
    brotli: options.precompress?.brotli ?? true,
    gzip: options.precompress?.gzip ?? false,
  };
  // Default `true` — the conservative choice. An unwanted Worker(SSR)
  // catch-all 404s; an unwanted Static catch-all serves stale shell.
  const userHasDefaultFetch = options.userHasDefaultFetch ?? true;
  const log = options.silent
    ? () => {}
    : (msg: string) => console.log(`[zeroship:zship] ${msg}`);

  if (!(await pathExists(distDir))) {
    throw new Error(`zship: dist dir not found at ${distDir}`);
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
      `zship: no files found under ${distDir} — did the build run?`
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

  // 4b. Pre-compress variants. For every asset whose content-type is
  //     compressible (text/*, JS, JSON, XML, SVG), emit `br` and/or
  //     `gzip` variants and record their compressed hashes alongside
  //     the identity entry. The gateway picks one at request time
  //     based on `Accept-Encoding`.
  //
  //     Variants smaller than identity get an entry; otherwise we
  //     skip — adding bytes to ship is the opposite of what we want.
  if (precompress.brotli || precompress.gzip) {
    let variantBlobCount = 0;
    let variantBytes = 0;
    for (const f of assetFiles) {
      const entry = assets[f.urlPath];
      if (!isCompressibleType(entry.content_type)) continue;
      const variants: Record<string, AssetVariant> = {};

      if (precompress.brotli) {
        const compressed = await compressBrotli(f.bytes);
        if (compressed.length < f.size) {
          const h = sha256Hex(compressed);
          if (!blobsByHash.has(h)) blobsByHash.set(h, compressed);
          variants["br"] = { hash: h, size: compressed.length };
          variantBlobCount += 1;
          variantBytes += compressed.length;
        }
      }
      if (precompress.gzip) {
        const compressed = compressGzip(f.bytes);
        if (compressed.length < f.size) {
          const h = sha256Hex(compressed);
          if (!blobsByHash.has(h)) blobsByHash.set(h, compressed);
          variants["gzip"] = { hash: h, size: compressed.length };
          variantBlobCount += 1;
          variantBytes += compressed.length;
        }
      }

      if (Object.keys(variants).length > 0) {
        entry.variants = variants;
      }
    }
    if (variantBlobCount > 0) {
      log(
        `pre-compressed ${variantBlobCount} variant blobs ` +
          `(${formatBytes(variantBytes)} compressed total; ` +
          `brotli=${precompress.brotli}, gzip=${precompress.gzip})`
      );
    }
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

  // 7. Build manifest.resources — auto-derived URL entries
  //    (asset prefix, common public files, prerendered HTML) plus the
  //    SSR / SPA-fallback catch-all. RPC procedure entries come from
  //    `rpcExtras` (via `manifest.ts`) and are merged on top.
  const autoResources = buildAutoResources({
    assetPrefix,
    assets,
    hasWorker: worker != null,
    userHasDefaultFetch,
  });

  // 8. Assemble the manifest. Rust's serde accepts both `null` and
  //    omitted-field for `Option<WorkerCode>`, but we omit the field
  //    entirely when there's no worker — this matches the spec's shape
  //    notes ("null/missing means SSG-only") and keeps the canonical
  //    JSON shorter.
  const userResources = options.rpcExtras?.resources ?? {};
  const mergedResources: Record<string, Record<string, unknown>> = {
    ...autoResources,
  };
  for (const [key, value] of Object.entries(userResources)) {
    mergedResources[key] = { ...(mergedResources[key] ?? {}), ...value };
  }
  const transformer = options.rpcExtras?.transformer ?? "json";

  const manifest: Manifest = {
    version: 1,
    assets,
    runtime_assets: {},
    asset_version: 0,
    sourcemaps,
    metadata: { compiler, built_at: builtAt },
  };
  if (worker != null) {
    manifest.worker = worker;
  }
  if (Object.keys(mergedResources).length > 0) {
    manifest.resources = mergedResources;
  }
  manifest.transformer = transformer;
  if (options.rpcExtras?.net && (options.rpcExtras.net.requests?.length ?? 0) > 0) {
    manifest.net = options.rpcExtras.net;
  }

  // 8b. Discover + bundle op.* migrations (PR4 A4). The committed `.ir.json`
  //     bytes are staged as content blobs (deduped by hash, exactly like assets)
  //     and contributed to `manifest.migrations` — consumed VERBATIM, never
  //     re-emitted. The packer COPIES; the bundle entry hash is the sha256 of the
  //     on-disk committed bytes.
  if (options.migrations !== false) {
    const migEntries = await discoverMigrations({
      root,
      migrationsDir: options.migrations?.dir,
      ownerApp: options.migrations?.ownerApp,
      recorderUrl: options.migrations?.recorderUrl,
      cliPath: options.migrations?.cliPath,
    });
    if (migEntries.length > 0) {
      manifest.migrations = migEntries.map((m) => ({ name: m.name, hash: m.hash }));
      for (const m of migEntries) {
        if (!blobsByHash.has(m.hash)) blobsByHash.set(m.hash, m.bytes);
      }
      log(`bundled ${migEntries.length} op.* migration(s)`);
    }

    // 8c. Carry the generated runtime schema descriptor (migration-first P4a).
    //     `gen-types` emits `<genTypesOut>/schema.runtime.json` by folding the
    //     migration set; the packer reads it VERBATIM, stages it as a content
    //     blob (deduped by hash, exactly like a migration), and records
    //     `manifest.runtime_descriptor = { hash }`. Absent (no migrations / not
    //     yet generated) → the slot is left undefined; pack still succeeds and
    //     the runtime installs no schema.
    const genTypesOut = options.migrations?.genTypesOut ?? GEN_TYPES_OUT_DEFAULT;
    const descriptorPath = resolve(root, genTypesOut, RUNTIME_DESCRIPTOR_FILE);
    let descriptorBytes: Buffer | undefined;
    try {
      descriptorBytes = await fs.readFile(descriptorPath);
    } catch {
      descriptorBytes = undefined; // no descriptor → leave the slot undefined
    }
    if (descriptorBytes != null) {
      const hash = sha256Hex(descriptorBytes);
      manifest.runtime_descriptor = { hash };
      if (!blobsByHash.has(hash)) blobsByHash.set(hash, descriptorBytes);
      log(`bundled runtime schema descriptor (${RUNTIME_DESCRIPTOR_FILE})`);
    }
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

/** Build the auto-derived URL-namespace resource entries. */
function buildAutoResources(opts: {
  assetPrefix: string;
  assets: Record<string, AssetEntry>;
  hasWorker: boolean;
  userHasDefaultFetch: boolean;
}): Record<string, Record<string, unknown>> {
  const out: Record<string, Record<string, unknown>> = {};
  const { assetPrefix, assets, hasWorker, userHasDefaultFetch } = opts;

  // Asset prefix → static, immutable cache (1y).
  if (Object.keys(assets).some((p) => p.startsWith(assetPrefix))) {
    // Strip the trailing slash so the resource key matches a glob path
    // shape that the gateway recognizes (single `*` consumes the rest).
    const key = `${assetPrefix.replace(/\/$/, "")}/*`;
    out[key] = {
      static: { try: ["$path"] },
      cache: {
        max_age: 31_536_000, // 1 year
        immutable: true,
      },
    };
  }

  // Common public files (favicon.ico, robots.txt, sitemap.xml).
  for (const path of ["/favicon.ico", "/robots.txt", "/sitemap.xml"]) {
    if (assets[path]) {
      out[path] = { static: { try: [path] } };
    }
  }

  // Prerendered HTML routes. Each `*.html` (except `/index.html`)
  // becomes an exact resource key serving the rendered file.
  const htmlPaths = Object.keys(assets).filter(
    (p) => p.endsWith(".html") && p !== "/index.html"
  );
  for (const htmlPath of htmlPaths) {
    const route = htmlPath.replace(/\.html$/, "");
    out[route] = { static: { try: [htmlPath] } };
  }

  // Catch-all under `/[...rest]`. The shape depends on whether the
  // app has a worker, whether the user exported `default.fetch`, and
  // whether an SPA shell exists.
  const catchAllKey = "/[...rest]";
  if (hasWorker) {
    if (userHasDefaultFetch) {
      // SSR catch-all: forward to the worker. No explicit action — a
      // URL-namespace resource without a routing action defaults to
      // worker SSR dispatch. We mark it `anon` + publicly_accessible
      // so the gateway's secure-by-default check doesn't reject it.
      out[catchAllKey] = {
        auth: "anon",
        publicly_accessible: true,
      };
    } else if (assets["/index.html"]) {
      // RPC-only app with SPA shell: catch-all serves index.html so
      // the browser router can claim unknown URLs.
      out[catchAllKey] = {
        static: { try: ["$path", "/index.html"] },
      };
    }
    // RPC-only without /index.html: no URL catch-all — gateway 404s
    // on anything outside the declared RPC procedure resources.
  } else {
    // SSG-only build.
    if (assets["/index.html"]) {
      out[catchAllKey] = {
        static: { try: ["$path", "/index.html"] },
      };
    } else if (assets["/404.html"]) {
      out[catchAllKey] = {
        static: { try: ["/404.html"] },
      };
    }
    // No SPA shell, no /404.html: no catch-all.
  }

  return out;
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
        `zship: asset ${path} references hash ${entry.hash} but the blob is missing`
      );
    }
    if (!isSha256Hex(entry.hash)) {
      throw new Error(
        `zship: asset ${path} hash ${entry.hash} is not lowercase 64-char sha256 hex`
      );
    }
    // Pre-compressed variants must also be valid + present.
    if (entry.variants) {
      for (const [enc, variant] of Object.entries(entry.variants)) {
        if (enc !== "br" && enc !== "gzip") {
          throw new Error(
            `zship: asset ${path} variant key ${JSON.stringify(enc)} is not in the v1 allow list (br, gzip)`
          );
        }
        if (!isSha256Hex(variant.hash)) {
          throw new Error(
            `zship: asset ${path} variant ${enc} hash ${variant.hash} is not lowercase 64-char sha256 hex`
          );
        }
        if (!blobsByHash.has(variant.hash)) {
          throw new Error(
            `zship: asset ${path} variant ${enc} hash ${variant.hash} has no corresponding blob`
          );
        }
      }
    }
  }
  // Every sourcemap key/value must exist as a blob (and be sha256 hex).
  for (const [k, v] of Object.entries(m.sourcemaps)) {
    if (!isSha256Hex(k) || !isSha256Hex(v)) {
      throw new Error(
        `zship: sourcemaps entry ${k} -> ${v} contains non-sha256 hex`
      );
    }
    if (!blobsByHash.has(k)) {
      throw new Error(
        `zship: sourcemap key ${k} has no corresponding asset blob`
      );
    }
    if (!blobsByHash.has(v)) {
      throw new Error(
        `zship: sourcemap value ${v} has no corresponding blob`
      );
    }
  }
  // Worker checks.
  if (m.worker != null) {
    if (!m.worker.entry || !(m.worker.entry in m.worker.modules)) {
      throw new Error(
        `zship: worker.entry ${JSON.stringify(m.worker.entry)} is not a key in worker.modules`
      );
    }
    for (const [spec, hash] of Object.entries(m.worker.modules)) {
      if (!isSha256Hex(hash)) {
        throw new Error(
          `zship: worker.modules[${spec}] hash ${hash} is not lowercase 64-char sha256 hex`
        );
      }
      if (!blobsByHash.has(hash)) {
        throw new Error(
          `zship: worker.modules[${spec}] hash ${hash} has no corresponding blob`
        );
      }
    }
  }
  // Every static-action `try` chain (with no captures or $path) must
  // point at a path that's a key in `assets`. We allow `$path`
  // (resolved at request time) and any path containing `[name]`
  // (glob captures).
  for (const [key, entry] of Object.entries(m.resources ?? {})) {
    const staticAction = (entry as Record<string, unknown>).static as
      | { try?: unknown }
      | undefined;
    if (!staticAction) continue;
    const tryChain = Array.isArray(staticAction.try) ? staticAction.try : [];
    for (const t of tryChain) {
      if (typeof t !== "string") continue;
      if (t === "$path" || t.includes("[")) continue;
      if (!(t in m.assets)) {
        throw new Error(
          `zship: resource ${JSON.stringify(key)}: static.try references ${t} but it's not in assets`
        );
      }
    }
  }
  // Every migration entry's hash must be valid sha256 hex AND have a blob (the
  // committed `.ir.json` bytes staged verbatim — the packer copies, never
  // re-emits). Mirrors the Rust `crates/bundle/src/unpack.rs` migration check.
  for (const mig of m.migrations ?? []) {
    if (!isSha256Hex(mig.hash)) {
      throw new Error(
        `zship: migration ${mig.name} hash ${mig.hash} is not lowercase 64-char sha256 hex`
      );
    }
    if (!blobsByHash.has(mig.hash)) {
      throw new Error(
        `zship: migration ${mig.name} hash ${mig.hash} has no corresponding blob`
      );
    }
  }
  // The runtime schema descriptor blob (migration-first P4a) — hash must be
  // valid sha256 hex AND have a staged blob. Mirrors the Rust
  // `crates/bundle/src/{manifest,unpack}.rs` descriptor checks.
  if (m.runtime_descriptor != null) {
    const h = m.runtime_descriptor.hash;
    if (!isSha256Hex(h)) {
      throw new Error(
        `zship: runtime_descriptor hash ${h} is not lowercase 64-char sha256 hex`
      );
    }
    if (!blobsByHash.has(h)) {
      throw new Error(
        `zship: runtime_descriptor hash ${h} has no corresponding blob`
      );
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

/**
 * Whether a given content-type benefits from text-style compression.
 *
 * Compressible:
 *   * `text/*`  (HTML, CSS, plain, etc.)
 *   * `application/javascript`, `application/json`, `application/xml`
 *   * `image/svg+xml` (SVG is XML)
 *
 * Skipped on already-compressed binary formats (PNG, JPEG, WebP,
 * AVIF, woff2, mp4, …) where re-compression wastes CPU and bytes.
 *
 * Strict prefix matching keeps the rule intentional — adding a new
 * compressible type is a one-line edit, not a heuristic to debug.
 */
function isCompressibleType(contentType: string): boolean {
  // Strip any charset / boundary / etc. parameters.
  const ct = contentType.split(";")[0].trim().toLowerCase();
  if (ct.startsWith("text/")) return true;
  if (ct === "application/javascript") return true;
  if (ct === "application/json") return true;
  if (ct === "application/xml") return true;
  if (ct === "image/svg+xml") return true;
  return false;
}

/** Brotli at quality 11 — slow at build time, optimal on the wire. */
async function compressBrotli(bytes: Buffer): Promise<Buffer> {
  return brotliCompressAsync(bytes, {
    params: {
      [zlibConstants.BROTLI_PARAM_QUALITY]: zlibConstants.BROTLI_MAX_QUALITY,
    },
  });
}

/** Gzip at level 9 — same trade-off as brotli but for legacy clients. */
function compressGzip(bytes: Buffer): Buffer {
  return gzipSync(bytes, { level: zlibConstants.Z_BEST_COMPRESSION });
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
