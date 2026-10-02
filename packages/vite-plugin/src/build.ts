import {
  type EnvironmentOptions,
  type Plugin,
  type ViteBuilder,
  BuildEnvironment,
} from "vite";
import { join, resolve, relative, isAbsolute } from "node:path";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type { TransformState } from "./transform.js";
import { isRuntimeModuleSpecifier } from "./node-compat.js";
import { emitZship } from "./zship.js";
import {
  rpcRegistryPlugin,
  SERVER_ENTRY_VIRTUAL_ID,
} from "./rpc-registry.js";
import {
  computeManifestExtras,
  type DiscoveredProcedure,
  type DiscoveredSchedule,
} from "./manifest.js";
import {
  ZEROSHIP_BUILD_TARGET,
  ZEROSHIP_ENVIRONMENT,
  ZEROSHIP_MAIN_FIELDS,
  ZEROSHIP_RESOLVE_CONDITIONS,
} from "./environment.js";
import { genTypesFromMigrations } from "./gen-types/index.js";
import {
  defaultProjectConfig,
  selectBuildTarget,
  type BuildTarget,
  type ProjectConfigHolder,
  type ResolvedProjectConfig,
} from "./project-config/index.js";

const HERE = resolve(fileURLToPath(import.meta.url), "..");

/** Public specifier for the client manifest virtual module. */
export const CLIENT_MANIFEST_VIRTUAL_ID = "virtual:zeroship/client-manifest";
/** Internal (\0-prefixed) id Vite uses for the same module. */
export const CLIENT_MANIFEST_RESOLVED_ID = "\0" + CLIENT_MANIFEST_VIRTUAL_ID;

/**
 * Specifier for the static-mode stub entry. Injected via `rollupOptions.input`
 * so Vite has something to chew on when the user has zero JS inputs (an
 * SSG-only project, e.g.). The matching `resolveId` + `load` resolve it
 * to an empty module, and `generateBundle` deletes the chunk before write.
 */
export const STATIC_STUB_VIRTUAL_ID = "virtual:zeroship/static-stub";
/** Internal (\0-prefixed) id Vite uses for the static stub. */
export const STATIC_STUB_RESOLVED_ID = "\0" + STATIC_STUB_VIRTUAL_ID;

/**
 * Build the Vite plugin that exposes the Vite client manifest as a
 * virtual ESM module to the worker. Resolved at the worker build's
 * `load` time, which `buildApp` runs AFTER the client build has written
 * `<root>/<distDir>/.vite/manifest.json` to disk.
 *
 * Usage in user SSR code:
 *
 *   import clientManifest from "virtual:zeroship/client-manifest";
 *   const entry = clientManifest["src/entry-client.tsx"];
 *   `<script type="module" src="/${entry.file}"></script>`;
 *
 * Behavior:
 *   - manifest exists  → emit `export default <inlined JSON>`
 *   - manifest missing → emit `export default {}` (graceful fallback;
 *                        client build hasn't run yet, dev mode, etc.)
 */
export function clientManifestPlugin(opts: {
  root: string;
  distDir: string;
}): Plugin {
  return {
    name: "zeroship:client-manifest",
    enforce: "pre",
    resolveId(id: string) {
      if (id === CLIENT_MANIFEST_VIRTUAL_ID) return CLIENT_MANIFEST_RESOLVED_ID;
      return null;
    },
    load(id: string) {
      if (id !== CLIENT_MANIFEST_RESOLVED_ID) return null;
      const path = resolve(opts.root, opts.distDir, ".vite", "manifest.json");
      if (!existsSync(path)) {
        // No client manifest: a server-only app, a client built without
        // `build.manifest`, or the dev archive, which builds no client.
        return "export default {};";
      }
      try {
        const json = JSON.parse(readFileSync(path, "utf8"));
        return `export default ${JSON.stringify(json)};`;
      } catch {
        return "export default {};";
      }
    },
  };
}

/** Compiler identifier used in metadata.compiler. Read from package.json. */
function getCompilerId(): string {
  try {
    const pkgPath = resolve(HERE, "../package.json");
    const pkg = JSON.parse(readFileSync(pkgPath, "utf8")) as {
      name?: string;
      version?: string;
    };
    return `${pkg.name ?? "@zeroship/vite-plugin"}@${pkg.version ?? "0.0.0"}`;
  } catch {
    return "@zeroship/vite-plugin";
  }
}

/**
 * Strip the leading `"use server"` directive from the rolled-up SSR
 * bundle. The Node-globals shim (`Buffer`, `setImmediate`, etc.) is
 * installed on every isolate by the Rust runtime before any user
 * module evaluates (see `crates/zeroship-runtime/src/core/init.rs`),
 * so the bundle needs no prelude.
 *
 * The directive itself is just a string expression at the top of the
 * module; if we leave it in place, it is a no-op but pollutes the
 * output. The synthetic server entry (loaded via
 * `virtual:zeroship/_server-entry`) emits `default.{fetch, rpc}` — the
 * runtime kernel dispatches to those directly.
 */
export function stripUseServer(bundle: string): string {
  return bundle.replace(/^"use server"\s*;\s*/, "");
}

/**
 * The worker build's graph check, and the absolute module ids the build read.
 *
 * A built worker may leave unbundled only the runtime's own modules, under
 * the spellings the runtime answers: `zeroship`, and `node:<name>` for a
 * module it implements. It refuses, naming the module:
 * - any other import left in the output, a bare `path` included;
 * - a CommonJS `require` that resolves to an external. The isolate has no
 *   `require`, and Rolldown would turn it into a shim that throws when the
 *   worker starts. The output's import lists never show such a require, so it
 *   is refused where it is resolved.
 *
 * A build that fails reports the modules it reached, the one that broke it
 * included, so the dev server can rebuild when that one is fixed.
 */
export function serverGraphPlugin(onDependencies?: (ids: string[]) => void): Plugin {
  return {
    name: "zeroship:server-dependencies",
    resolveId: {
      order: "pre",
      async handler(source, importer, options) {
        if (options?.kind !== "require-call") return null;
        const resolved = await this.resolve(source, importer, {
          ...options,
          kind: options.kind,
          skipSelf: true,
        });
        if (resolved?.external) {
          this.error(
            `server executable requires ${source} without bundling it: the runtime has no ` +
              "`require`, so the worker would throw when it starts. Let the build bundle it, " +
              `or point ${source} at a stub module with \`resolve.alias\`.`,
          );
        }
        return resolved;
      },
    },
    buildEnd() {
      if (!onDependencies) return;
      onDependencies([...new Set([...this.getModuleIds()]
        .map(id => id.split("?")[0])
        .filter(id => isAbsolute(id)))].sort());
    },
    generateBundle(_options, bundle) {
      for (const output of Object.values(bundle)) {
        if (output.type !== "chunk") continue;
        for (const imported of [...output.imports, ...output.dynamicImports]) {
          if (bundle[imported]?.type !== "chunk" && !isRuntimeModuleSpecifier(imported)) {
            this.error(`server executable contains an unbundled import: ${imported}`);
          }
        }
      }
    },
  };
}

/**
 * Probe a server-entry source string for an own `default.fetch`.
 *
 * Internal helper — we call this on the user's untransformed entry
 * source to decide which catch-all rule the .zship emitter writes
 * (Worker(SSR) when the user wrote their own fetch, Static SPA fallback
 * otherwise). The synthetic SSR entry ALWAYS emits a `fetch:` key on
 * its default (it surfaces the user's fetch when present, else
 * undefined), so probing the synthetic output is meaningless — we
 * inspect the USER's source.
 *
 * The synthetic entry always emits `fetch:` on default, so this probe
 * detects whether the user actually wrote a `fetch` handler.
 * Heuristics (kept simple — full AST analysis would over-fit):
 *
 *   - `export default function fetch(…)`        → true
 *   - `export default { fetch …}`               → true (object short-
 *                                                 hand or property
 *                                                 colon form)
 *   - `export default <ident>` referring to a   → matched conservatively
 *     module-level `fetch` symbol                  via standalone
 *                                                  `function fetch(…)` /
 *                                                  `export function
 *                                                  fetch(…)`
 *   - `export default { rpc: {…} }` ONLY        → false (RPC-only app)
 *
 * Conservative on any read failure / ambiguity: returns true (an
 * unwanted Worker(SSR) 404s; an unwanted Static catch-all serves stale
 * shell on intended SSR routes).
 */
/**
 * Return the source text of an `export default { ... }` object literal,
 * brace-matched from its opening `{` to its true closing `}`, or null when the
 * default export is not an object literal.
 *
 * Quote- and template-aware, because a `}` inside a string is not a closing
 * brace. Not a parser: it does not track regex literals, and a `}` inside one
 * would end the scan early. That failure direction is safe: it fails toward a
 * SHORTER block, i.e. toward the conservative `return true` below.
 */
/**
 * Replace everything nested deeper than the outermost object's own level with
 * spaces, preserving length and the outer braces. Lets a flat key regex run
 * against depth-1 keys only. Quote-aware for the same reason as the matcher.
 */
function blankNestedLevels(block: string): string {
  const out = block.split("");
  let depth = 0;
  let quote: string | null = null;
  for (let i = 0; i < block.length; i++) {
    const ch = block[i];
    if (quote) {
      if (ch === "\\") {
        if (depth > 1) out[i] = out[i + 1] = " ";
        i++;
        continue;
      }
      if (ch === quote) quote = null;
      if (depth > 1) out[i] = " ";
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") {
      quote = ch;
      if (depth > 1) out[i] = " ";
      continue;
    }
    if (ch === "{" || ch === "[") {
      depth++;
      if (depth > 1) out[i] = " ";
      continue;
    }
    if (ch === "}" || ch === "]") {
      if (depth > 1) out[i] = " ";
      depth--;
      continue;
    }
    if (depth > 1) out[i] = " ";
  }
  return out.join("");
}

function matchDefaultObjectLiteral(stripped: string): string | null {
  const m = /export\s+default\s+\{/.exec(stripped);
  if (!m) return null;
  const open = stripped.indexOf("{", m.index);
  let depth = 0;
  let quote: string | null = null;
  for (let i = open; i < stripped.length; i++) {
    const ch = stripped[i];
    if (quote) {
      if (ch === "\\") i++;
      else if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") {
      quote = ch;
      continue;
    }
    if (ch === "{") depth++;
    else if (ch === "}") {
      depth--;
      if (depth === 0) return stripped.slice(open, i + 1);
    }
  }
  // Unbalanced (truncated source): treat as not-an-object-literal so the
  // caller falls through to its conservative arms rather than reading a
  // half-object.
  return null;
}

export function probeUserDefaultExport(source: string): boolean {
  const stripped = source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:])\/\/.*$/gm, "$1");

  // No default export at all → no fetch handler the runtime can use.
  // Match `export default` anywhere on any line — not anchored to
  // line-start (a single-line source like
  // `import x from "./x"; export default x;` would otherwise miss).
  if (!/(?:^|[\s;])export\s+default\b/.test(stripped)) return false;

  // `export default function fetch(...)` / `export default async function fetch(...)`
  // Function shape — the function IS the WinterCG handler.
  if (/export\s+default\s+(?:async\s+)?function\s+fetch\b/.test(stripped)) {
    return true;
  }

  // `export default { ... }` — look for a `fetch:` or `fetch(...)` key
  // inside the object literal.
  //
  // THIS BRACE-MATCHES, and it must: a regex cannot find the end of the
  // default-export object. Greedy (`\{[\s\S]*\}`) runs to the LAST `}` in
  // the file, so any `fetch(` call anywhere in the module (e.g. inside an
  // unrelated outbound action) false-positives as "user owns routing" and
  // a static app ships with a Worker(SSR) catch-all. Lazy (`[\s\S]*?`) is
  // NOT the fix either: it stops at the FIRST `}`, so
  // `export default { a: { b: 1 }, fetch: h }` would miss the real top-level
  // `fetch` and serve a stale shell on a genuine SSR route — precisely the
  // failure the conservative default exists to avoid.
  const defaultBlock = matchDefaultObjectLiteral(stripped);
  if (defaultBlock) {
    // Depth-1 projection: nested object/array bodies are blanked out so the
    // key regex below can only ever see the literal's OWN keys. A nested
    // `fetch` key (`rpc: { helpers: { fetch } }`) is a realistic shape that
    // must NOT route the app as Worker(SSR).
    const block = blankNestedLevels(defaultBlock);
    // Top-level keys: `fetch:` (property), `fetch(` (method shorthand),
    // `fetch,` / `fetch}` (shorthand from a binding), or `"fetch":`.
    if (/(?:^|[,{\s])(?:fetch|["']fetch["'])\s*[:(,}]/.test(block)) return true;
    // No fetch key in the default object — RPC-only / schema-only app.
    return false;
  }

  // `export default <identifier>` — we can't cheaply decide without an
  // AST; check whether the source defines a top-level `fetch` symbol.
  // (`function fetch(...)` / `const fetch = ...` / `export function fetch(...)`)
  if (/(?:^|\n)\s*(?:export\s+)?(?:async\s+function|function|const|let|var)\s+fetch\b/.test(stripped)) {
    return true;
  }

  // Default export of some other shape we can't probe (a class, an
  // imported binding, a call result). Conservative: assume yes.
  return true;
}

/** Find server entry point in project */
export function findServerEntry(root: string, explicit?: string): string | null {
  if (explicit && existsSync(resolve(root, explicit))) return resolve(root, explicit);
  for (const candidate of [
    "src/server.ts",
    "src/server.js",
    "server.ts",
    "server.js",
    "src/index.server.ts",
    "src/index.ts",
    "src/index.js",
    "index.ts",
    "index.js",
  ]) {
    const p = resolve(root, candidate);
    if (existsSync(p)) return p;
  }
  return null;
}

/**
 * Pin the worker environment's build to the worker contract.
 *
 * The `zeroship` environment inherits the app's top-level `define`, `resolve`
 * and `build` like any non-client environment, so the worker sees the app's
 * defines and aliases. The options below shape the deployable worker and are
 * owned here. They are ASSIGNED on the options object Vite hands
 * `configEnvironment`: a returned partial would be deep-merged, which unions an
 * object `input` with the app's and concatenates `resolve.conditions`.
 *
 * Resolution is the dev environment's own (`ZEROSHIP_RESOLVE_CONDITIONS`,
 * `ZEROSHIP_MAIN_FIELDS`, and Vite's server `builtins`), so dev and the built
 * worker resolve every package to the same build. `builtins` only decides a
 * Node built-in `nodeCompatPlugin` does not know: it maps or keeps every one it
 * knows before resolution, and an unknown one becomes an external the graph
 * check refuses by name.
 */
export function applyWorkerBuildOptions(environment: EnvironmentOptions, clientOutDir: string): void {
  environment.consumer = "server";
  environment.resolve = {
    ...environment.resolve,
    conditions: [...ZEROSHIP_RESOLVE_CONDITIONS],
    mainFields: [...ZEROSHIP_MAIN_FIELDS],
    noExternal: true,
  };
  // The runtime supplies a real `process.env` (from `worker_env` and the
  // app's exposed secrets), so every spelling of it stays live: Vite's own
  // replacement would turn `globalThis.process.env` into `{}`. `NODE_ENV` is
  // replaced statically in each spelling, which is what removes development
  // branches, and never with the value of the shell that runs the build.
  environment.keepProcessEnv = true;
  const production = JSON.stringify("production");
  environment.define = {
    ...environment.define,
    "process.env.NODE_ENV": production,
    "global.process.env.NODE_ENV": production,
    "globalThis.process.env.NODE_ENV": production,
  };
  const {
    rollupOptions: legacyRollupOptions,
    rolldownOptions,
    ...build
  } = environment.build ?? {};
  // The app's `external` shapes its client. The worker bundles everything
  // but the runtime's own modules, which its resolvers mark external.
  const { external: _clientExternal, ...inherited } = rolldownOptions ?? legacyRollupOptions ?? {};
  environment.build = {
    ...build,
    // The packer walks the CLIENT environment's outDir (`build.dist`) and
    // reads the worker from its `server` directory.
    outDir: join(clientOutDir, "server"),
    emptyOutDir: false,
    // Public files are client assets, never worker modules.
    copyPublicDir: false,
    manifest: false,
    ssrManifest: false,
    sourcemap: false,
    license: false,
    minify: true,
    target: ZEROSHIP_BUILD_TARGET,
    ssr: true,
    rolldownOptions: {
      ...inherited,
      input: { index: SERVER_ENTRY_VIRTUAL_ID },
      // Rolldown's `node` platform imports `node:module` for its CommonJS
      // helpers, which the runtime does not provide.
      platform: "browser",
      // One module. A lazy procedure's module still initialises on its first
      // call inside it, and `stripUseServer` post-processes `index.js` only.
      output: { format: "esm", entryFileNames: "index.js", codeSplitting: false },
    },
  };
}

/**
 * Build the worker in the builder's `zeroship` environment.
 *
 * A discovery pass builds the app's server entry without writing, which
 * transforms every `"use server"` module the entry reaches and records its
 * procedures; the synthetic entry the environment build then loads imports
 * each declaring module from that record. Both passes use the environment's
 * resolved config, so the app's plugins apply to both.
 *
 * `overrides`, when given, build the worker in a fresh environment with them
 * merged over the worker's own options. The discovery pass writes nothing,
 * so they do not apply to it.
 */
async function buildWorker(
  builder: ViteBuilder,
  entry: string,
  overrides?: EnvironmentOptions,
): Promise<void> {
  const environment = builder.environments[ZEROSHIP_ENVIRONMENT];
  if (environment == null) {
    throw new Error(
      `[zeroship] the app builder has no "${ZEROSHIP_ENVIRONMENT}" environment to build the worker in`,
    );
  }
  const config = environment.getTopLevelConfig();
  const fresh = async (options: EnvironmentOptions): Promise<BuildEnvironment> => {
    const built = new BuildEnvironment(ZEROSHIP_ENVIRONMENT, config, { options });
    await built.init();
    return built;
  };
  await builder.build(await fresh({
    build: { write: false, rolldownOptions: { input: { index: entry } } },
  }));
  const worker = overrides ? await fresh(overrides) : environment;
  await builder.build(worker);
  const { root, build } = worker.config;
  const entryPath = resolve(root, build.outDir, "index.js");
  writeFileSync(entryPath, stripUseServer(readFileSync(entryPath, "utf8")), "utf8");
}

/** A worker build that failed, and the sources it read before it did. */
export class WorkerBuildError extends Error {
  constructor(cause: unknown, readonly dependencies: string[]) {
    super(cause instanceof Error ? cause.message : String(cause), { cause });
    this.name = "WorkerBuildError";
  }
}

/** What the dev archive's worker build discovered and read. */
export interface DevWorkerBuild {
  state: TransformState;
  dependencies: string[];
}

/**
 * The build half's API, which the dev archive reaches through the
 * `zeroship:build` plugin of the builder it creates.
 */
export interface ZeroshipBuildApi {
  /**
   * Build the worker alone, as `vite build` builds it, into `outDir`, with
   * `NODE_ENV` replaced by `nodeEnv`. Throws a `WorkerBuildError`.
   */
  buildDevWorker(
    builder: ViteBuilder,
    options: { outDir: string; nodeEnv: string },
  ): Promise<DevWorkerBuild>;
}

/** The name of the plugin that carries `ZeroshipBuildApi`. */
export const BUILD_PLUGIN_NAME = "zeroship:build";

/** Restrict a plugin to the worker build. */
function workerBuildOnly(plugin: Plugin): Plugin {
  return {
    ...plugin,
    apply: "build",
    sharedDuringBuild: true,
    applyToEnvironment: (environment) => environment.name === ZEROSHIP_ENVIRONMENT,
  };
}

/**
 * The build half of `zeroship()`.
 *
 * `vite build` builds the app through Vite's app builder: this plugin's
 * `config` hook opts the app into it, and its `buildApp` hook builds the
 * client environment, then the worker in the `zeroship` environment, then
 * packs the `.zship`. The worker is therefore compiled from the app's own
 * config and plugins, as server code is in dev. The plugins are shared
 * across the builder's environments, so the procedures the client and worker
 * transforms discover reach the manifest.
 */
export function buildPlugins(
  state: TransformState,
  project: ProjectConfigHolder,
  appLabel?: string,
): Plugin[] {
  const { serverFunctionMap } = state;
  // Every build shape comes from `zeroship.jsonc` (or its schema defaults
  // when there is no file). These are project config, not plugin options,
  // because the Rust CLI also needs them and cannot read `vite.config.ts`.
  let projectConfig: ResolvedProjectConfig = defaultProjectConfig();
  // Which declared app this build is, and the databases it uses. A workspace
  // declaring one app implies it; one declaring several must be told which.
  let buildTarget: BuildTarget = { label: null, databases: [] };
  let root = "";
  let isDev = false;
  // The client environment's resolved `outDir`: what the packer walks.
  let clientOutDir = "";
  // The app's server entry. The synthetic entry imports it.
  let serverEntry: string | null = null;
  // Whether `buildApp` is running this build. A programmatic `vite.build()`
  // builds one environment and never calls it. Reset by every config
  // resolution, because an inline config hands the same plugin objects to
  // every build it drives.
  let buildAppRan = false;
  // Per config resolution: the client environment's outDir option and the
  // worker environment's options, which `configEnvironment` sees one
  // environment at a time.
  let clientOutDirOption: string | undefined;
  let workerEnvironment: EnvironmentOptions | undefined;
  // Whether the user's SSR entry source contains `export default`.
  // Probed before Rollup runs so it isn't confused by the generated server
  // entry's default export. Conservative default = true (emit Worker(SSR)
  // catch-all when in doubt; better to 404 than serve stale shell).
  let userHasDefaultFetch = true;
  // Vite's resolved mode — drives the manifest emitter's
  // production-mode gate (every procedure must have an explicit `id`
  // when shipping a production build).
  let viteMode: "production" | "development" = "production";
  // The absolute module ids each pass of the worker build read, in order:
  // the discovery pass, then the worker pass.
  let passDependencies: string[][] = [];
  // Vite's resolved logger. Manifest warnings (notably the fail-closed
  // auth notice, which names procedures that will 401 once deployed) go
  // through it so they land in a normal `vite build` / `pnpm build`
  // transcript rather than a bare console.warn.
  let logger: { warn: (msg: string) => void } | undefined;

  /**
   * Migration-first gen-types. Before anything is built, fold the committed
   * `.ts` migration set into the typed `env.db` surface (`env.db.ts` +
   * `schema.runtime.json`) via the in-process `gen-types` library
   * (`genTypesFromMigrations` — no subprocess).
   *
   * In production (`viteMode === "production"`) we run `--check`: a generated
   * artifact check that hard-fails the build when `env.db.ts` or
   * `schema.runtime.json` no longer track the migrations.
   * In a non-production build we REGENERATE (write) so a local
   * `vite build --mode development` refreshes the committed types.
   *
   * Type activation is app-level: the emitted `env.db.ts` is committed under
   * `generated/zeroship/` by default and included by the app tsconfig. See
   * `gen-types/index.ts`.
   *
   * The dev server's `hotUpdate` handles regeneration in dev; a database with
   * no migrations dir on disk is skipped.
   */
  async function generateTypes(): Promise<void> {
    const isProd = viteMode === "production";
    for (const database of buildTarget.databases) {
      const migrationsAbs = resolve(root, database.migrations);
      // No migrations dir → nothing to generate (a database may ship none).
      if (!existsSync(migrationsAbs)) continue;
      const outDir = resolve(root, database.out);
      try {
        // Production: generated-artifact check (a HARD drift gate — no binary to be
        // absent, so drift is always caught). Non-production: regenerate (write) so a
        // local `vite build --mode development` refreshes the committed types.
        // The LABEL and the PRIMARY FLAG come from `zeroship.jsonc` through
        // `selectBuildTarget`, not from the fold: the emitted module keys
        // `EnvDatabases` on the label, and only the primary declares
        // `Env.db`.
        const { status } = await genTypesFromMigrations(migrationsAbs, outDir, {
          label: database.label,
          primary: database.primary,
          check: isProd,
        });
        console.log(
          status === "checked"
            ? `[zeroship] gen-types --check: ${database.label} env.db.ts + schema.runtime.json track the migrations`
            : status === "unchanged"
            ? `[zeroship] gen-types: ${database.label} env.db.ts + schema.runtime.json already match the migrations`
            : `[zeroship] gen-types: regenerated ${database.label} env.db.ts + schema.runtime.json from the migrations`,
        );
      } catch (e) {
        // A drift / load / fold failure is a real build error — surface it.
        console.error(`[zeroship] gen-types failed for ${database.label}: ${(e as Error).message}`);
        throw e;
      }
    }
  }

  /** Build the worker, when the app is in full mode and has a server entry. */
  async function buildServer(builder: ViteBuilder): Promise<void> {
    if (projectConfig.build.mode === "static") return;
    const entry = serverEntry;
    if (!entry) {
      console.warn("[zeroship] no server entry found — skipping server bundle");
      return;
    }

    // Probe the user's untransformed entry source for `export default`.
    // The synthetic SSR entry ALWAYS exports a default, so we can't
    // probe the bundled output for this — we need the user's source.
    // This drives the .zship's catch-all rule choice (Worker(SSR) vs
    // Static SPA fallback). Conservative default is true on read
    // failure: an unwanted Worker(SSR) 404s while an unwanted Static
    // catch-all serves stale shell on intended SSR routes.
    try {
      const entrySource = readFileSync(entry, "utf8");
      userHasDefaultFetch = probeUserDefaultExport(entrySource);
    } catch {
      userHasDefaultFetch = true;
    }

    console.log(`[zeroship] building server bundle from ${relative(root, entry)}`);
    await buildWorker(builder, entry);

    const totalFns = [...serverFunctionMap.values()].reduce((sum, fns) => sum + fns.size, 0);
    console.log(
      `[zeroship] server bundle complete — ${serverFunctionMap.size} modules, ${totalFns} server functions`
    );
  }

  /** Pack the built client output and worker into the `.zship`. */
  async function emitArchive(): Promise<void> {
    try {
      // Compute the manifest's resource tree (auto-derived RPC
      // procedure entries plus any user-declared resources from
      // `src/server/config.ts`). WireIds are a pure function of
      // current source — explicit `fn.config.id` wins, otherwise
      // the bare `<exportName>` is the default (rejected in
      // production by the manifest emitter).
      //
      // Schemas (Zod) live on the procedures themselves at runtime;
      // the synthetic SSR entry's dispatch validates against them.
      // The manifest never carries JSONSchemas.
      const procedures: DiscoveredProcedure[] = state.discoveredProcedures.map(
        (p) => ({
          filePath: p.filePath,
          exportName: p.exportName,
          moduleSlug: p.moduleSlug,
          kind: p.kind,
          isStream: p.isStream,
          config: p.config,
          moduleConfig: p.moduleConfig,
        }),
      );
      const schedules: DiscoveredSchedule[] = (state.discoveredSchedules ?? []).map(
        (s) => ({
          filePath: s.filePath,
          name: s.name,
          workflowName: s.workflowName,
          schedule: s.schedule,
          input: s.input,
          overlap: s.overlap,
          catchUp: s.catchUp,
        }),
      );

      // Workflow names the manifest must DECLARE. Without them the control
      // plane refuses every `env.workflows.<Name>.start(...)` with
      // "workflow '<Name>' is not declared by the active deploy"
      // -- an app that deploys, serves, and cannot run a single workflow.
      // Sorted and de-duplicated so the same source always packs the same
      // bytes; the discovery pass and the worker build both transform the
      // same files.
      const workflowNames = [
        ...new Set((state.discoveredWorkflows ?? []).map((w) => w.exportName)),
      ].sort();

      const extras = await computeManifestExtras({
        root,
        procedures,
        schedules,
        workflowNames,
        mode: viteMode,
        onWarn: (msg) => {
          const line = `[zeroship:manifest] ${msg}`;
          if (logger) logger.warn(line);
          else console.warn(line);
        },
      });

      await emitZship({
        root,
        runtimeDate: projectConfig.runtime_date,
        distDir: clientOutDir,
        // The path the CLI will upload. It is x-cli-read: the packer writes
        // it and `zeroship deploy` reads it, which is exactly the
        // producer/consumer split that put it in the file.
        outputPath: resolve(root, projectConfig.build.output),
        compiler: getCompilerId(),
        userHasDefaultFetch,
        rpcExtras: {
          resources: extras.resources,
          transformer: extras.transformer,
          net: extras.net,
          schedules: extras.schedules,
          workflows: extras.workflows,
        },
        // Carry one generated runtime schema descriptor
        // (`schema.runtime.json`) per database the app declares, as the
        // gen-types step emitted them. Migration documents are
        // applied through the migration service and are not packed into
        // .zship.
        databases: buildTarget.databases.map((database) => ({
          label: database.label,
          id: database.id,
          primary: database.primary,
          migrations: database.migrations,
          out: database.out,
        })),
      });
    } catch (e) {
      console.error(`[zeroship] failed to emit .zship: ${(e as Error).message}`);
      throw e;
    }
  }

  // Whether we injected the empty stub input into the CLIENT environment.
  // Set in `config`. Two situations need it:
  //   - static mode (SSG): the user ships prerendered HTML themselves and
  //     has no JS entry, so Vite's "needs at least one input" check fails.
  //   - full mode, SERVER-ONLY app (no `index.html`, no `rollupOptions.input`,
  //     e.g. a pure fetch/RPC backend): the client build defaults to
  //     resolving `index.html` and errors `UNRESOLVED_ENTRY`.
  // In both cases we feed an empty virtual module so the client build
  // produces zero asset output, then delete the stub chunk in
  // `generateBundle`. The stub is the CLIENT environment's input only: the
  // worker's input is its own.
  let stubInjected = false;

  const api: ZeroshipBuildApi = {
    async buildDevWorker(builder, options) {
      const entry = serverEntry;
      if (!entry) throw new Error("[zeroship] the app has no server entry to build the worker from");
      const nodeEnv = JSON.stringify(options.nodeEnv);
      passDependencies = [];
      try {
        await buildWorker(builder, entry, {
          define: {
            "process.env.NODE_ENV": nodeEnv,
            "global.process.env.NODE_ENV": nodeEnv,
            "globalThis.process.env.NODE_ENV": nodeEnv,
          },
          build: { outDir: options.outDir },
        });
      } catch (error) {
        // A pass that failed partway read only part of the graph, so every
        // source any pass read stays a dependency.
        throw new WorkerBuildError(error, [...new Set(passDependencies.flat())].sort());
      }
      return { state, dependencies: passDependencies.at(-1) ?? [] };
    },
  };

  const orchestrator: Plugin = {
    name: BUILD_PLUGIN_NAME,
    api,
    // One instance across the builder's environments: `buildApp` reads the
    // state every environment's transforms wrote.
    sharedDuringBuild: true,

    /**
     * Opt `vite build` into the app builder, and satisfy Vite's "needs at
     * least one input" check by injecting a virtual entry that resolves to an
     * empty module when the client would otherwise have NO input. The
     * matching `generateBundle` below deletes the empty chunk so the .zship
     * emitter doesn't catalog a `_empty-<hash>.js` asset.
     *
     * We inject only when the user has NOT configured a client input —
     * neither an explicit input NOR a root `index.html` (Vite's implicit
     * default entry). If either exists, the client build has real work to do
     * and we leave it alone (CSR/SSR/SSG-with-HTML).
     */
    config(userConfig, env) {
      // The FIRST hook with a root. Vite resolves it as `config.root || cwd`,
      // and so do we -- reading the file under a different root than Vite ends
      // up using is how a build silently uses a sibling app's settings.
      root = resolve(userConfig?.root ?? process.cwd());
      projectConfig = project.load(root);
      buildTarget = selectBuildTarget(projectConfig, appLabel);
      const isBuild = env.command === "build";
      buildAppRan = false;
      clientOutDirOption = undefined;
      workerEnvironment = undefined;
      if (isBuild && userConfig.build?.watch) {
        throw new Error(
          "[zeroship] vite build --watch is not supported: the worker and the .zship are built " +
            "once per `vite build`. Use `pnpm dev` for a rebuilding server.",
        );
      }
      const workerBuild = userConfig.environments?.[ZEROSHIP_ENVIRONMENT]?.build;
      if (
        isBuild &&
        (workerBuild?.rollupOptions?.external != null || workerBuild?.rolldownOptions?.external != null)
      ) {
        throw new Error(
          `[zeroship] environments.${ZEROSHIP_ENVIRONMENT}.build.rollupOptions.external is not ` +
            "supported: the worker bundles everything but the runtime's own modules, so an external " +
            "would leave an import the runtime cannot answer. Let the build bundle the module, or point " +
            "it at a stub module with `resolve.alias`.",
        );
      }
      // `builder` makes `vite build` build every environment through
      // `buildApp` instead of the client alone.
      const builder = isBuild ? { builder: {} } : {};
      const clientBuild = userConfig.environments?.client?.build;
      const hasExplicitInput =
        userConfig.build?.rollupOptions?.input != null ||
        userConfig.build?.rolldownOptions?.input != null ||
        clientBuild?.rollupOptions?.input != null ||
        clientBuild?.rolldownOptions?.input != null;
      if (hasExplicitInput) return builder;
      // Vite auto-uses `<root>/index.html` as the entry when present.
      if (existsSync(resolve(root, "index.html"))) return builder;
      // No client entry of any kind — inject the empty stub so the client
      // build has something to chew on and doesn't 404 on index.html. The
      // dev server's dependency scan reads the same client input.
      stubInjected = true;
      return {
        ...builder,
        environments: {
          client: {
            build: {
              rollupOptions: {
                input: { __zeroship_static_stub: STATIC_STUB_VIRTUAL_ID },
              },
            },
          },
        },
      };
    },

    configEnvironment(name, environment, env) {
      if (env.command !== "build") return;
      if (name === "client") clientOutDirOption = environment.build?.outDir ?? "dist";
      if (name === ZEROSHIP_ENVIRONMENT) workerEnvironment = environment;
      if (workerEnvironment != null && clientOutDirOption != null) {
        applyWorkerBuildOptions(workerEnvironment, clientOutDirOption);
        workerEnvironment = undefined;
      }
    },

    /**
     * Provide the virtual stub module the `config` hook injected above.
     * Resolves to an empty ESM module — Rollup emits a chunk for it
     * which `generateBundle` then deletes.
     */
    resolveId(id: string) {
      if (!stubInjected) return null;
      if (id === STATIC_STUB_VIRTUAL_ID) return STATIC_STUB_RESOLVED_ID;
      return null;
    },
    load(id: string) {
      if (!stubInjected) return null;
      if (id !== STATIC_STUB_RESOLVED_ID) return null;
      // Empty module — no exports, no side effects.
      return "// zeroship empty client stub\n";
    },
    /**
     * After Rollup builds the (empty) stub chunk, delete its output so
     * the .zship doesn't end up shipping a `_empty-<hash>.js`.
     */
    generateBundle(_options: unknown, bundle: Record<string, { name?: string }>) {
      if (!stubInjected) return;
      for (const [filename, chunk] of Object.entries(bundle)) {
        if (chunk.name === "__zeroship_static_stub") {
          delete bundle[filename];
        }
      }
    },

    configResolved(config) {
      root = config.root;
      projectConfig = project.load(root);
      buildTarget = selectBuildTarget(projectConfig, appLabel);
      isDev = config.command === "serve";
      logger = config.logger;
      // Vite's ResolvedConfig.mode reflects the `--mode` flag
      // (`production` for `vite build` by default; `development` for
      // `vite build --mode development`). Anything other than the two
      // recognized values falls back to `production` for the gate —
      // custom modes (e.g. "staging") are still real ships.
      viteMode = config.mode === "development" ? "development" : "production";
      // The CLIENT environment's outDir, read from its own options: this hook
      // also runs for the worker environment's config, whose `build` is the
      // worker's.
      clientOutDir = resolve(root, config.environments.client.build.outDir);
      serverEntry = findServerEntry(root, projectConfig.build.serverEntry);
      // ONE dist dir, not two. `build.dist` in zeroship.jsonc is what the
      // packer walks and what any other tool would read; Vite's
      // `build.outDir` is what the client build writes. If they disagree the
      // packer walks a directory the build did not fill, and the failure is a
      // `.zship` that is missing assets rather than an error -- so this is an
      // error, named on both sides. Only checked when a file was actually
      // found: with no file both sides are "dist" by construction.
      if (project.path() != null && resolve(root, projectConfig.build.dist) !== clientOutDir) {
        throw new Error(
          "[zeroship] build.dist in " + project.path() + " is " +
            JSON.stringify(projectConfig.build.dist) +
            " but Vite build.outDir resolves to " + JSON.stringify(relative(root, clientOutDir)) +
            ". The packer walks build.dist and Vite fills build.outDir; two spellings of one " +
            "directory is how a .zship ends up missing every asset. Make them the same.",
        );
      }
    },

    /**
     * Build the app: types, then the client, then the worker, then the
     * `.zship`. The client goes first because the worker's
     * `virtual:zeroship/client-manifest` reads the client's manifest from
     * disk, and the archive is packed last because it walks both outputs.
     */
    async buildApp(builder) {
      buildAppRan = true;
      await generateTypes();
      await builder.build(builder.environments.client);
      await buildServer(builder);
      await emitArchive();
    },

    /**
     * A programmatic `vite.build()` builds the client environment without
     * `buildApp`, so it would leave a client build and no worker or archive.
     * Refuse it by name, before the client writes anything. A tool that drives
     * the builder itself and builds only the worker environment is not
     * refused.
     */
    buildStart() {
      if (isDev || buildAppRan) return;
      if (this.environment?.name !== "client") return;
      throw new Error(
        "[zeroship] the worker and the .zship are built by the app builder. Run `vite build`, " +
          "or `createBuilder(config).buildApp()` when building programmatically: a programmatic " +
          "`vite.build()` builds a single environment.",
      );
    },
  };

  const serverEntryRel = (): string => {
    if (serverEntry == null) {
      throw new Error("[zeroship] the worker build started before its server entry was found");
    }
    return serverEntry.replace(/\\/g, "/");
  };

  return [
    orchestrator,
    workerBuildOnly(rpcRegistryPlugin({
      get userEntryRel() {
        return serverEntryRel();
      },
      state,
    })),
    workerBuildOnly(clientManifestPlugin({
      get root() {
        return root;
      },
      get distDir() {
        return relative(root, clientOutDir);
      },
    })),
    workerBuildOnly(serverGraphPlugin((ids) => {
      passDependencies.push(ids);
    })),
  ];
}
