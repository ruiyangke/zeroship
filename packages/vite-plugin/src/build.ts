import {
  type EnvironmentOptions,
  type Plugin,
  type ViteBuilder,
  BuildEnvironment,
  build as viteBuild,
  defaultClientConditions,
  defaultClientMainFields,
} from "vite";
import { join, resolve, relative, isAbsolute } from "node:path";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { transformPlugin, type TransformState } from "./transform.js";
import { nodeCompatPlugin, isRuntimeModuleSpecifier } from "./node-compat.js";
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
import { zeroshipModulePlugin } from "./zeroship-module.js";
import { ZEROSHIP_ENVIRONMENT } from "./environment.js";
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
 * Build the InlineConfig the dev archive's server build passes to
 * `viteBuild()`. `vite build` does not use it: it builds the worker in the
 * app builder's `zeroship` environment (see `applyWorkerBuildOptions`).
 *
 * Exposed (and exported) so tests can verify the shape without spinning
 * up a real Vite environment. Notable invariants:
 *   - `publicDir: false` - the SSR outDir is `<build.dist>/server/`, and Vite's
 *     default would copy `public/*` into it. Those copies then end up
 *     cataloged as `worker.modules` entries, which is wrong: public
 *     files are static assets, not worker code. The client build keeps
 *     its `publicDir` so they still ship to `dist/<root>/`.
 *   - `noExternal: true` — bundle every dep (npm packages have no
 *     ESM resolver inside the V8 runtime).
 *   - `target: "webworker"` — picks the right export-conditions map.
 *   - `entryFileNames: "index.js"` — deterministic name; the .zship
 *     emitter uses it as the worker entry.
 *   - `build.ssr: true` + `rollupOptions.input` — when ssrEntry is a
 *     virtual specifier (e.g. `virtual:zeroship/_server-entry`), Vite's
 *     own SSR-string handler prepends `path.resolve(root, …)` and
 *     mangles it. Threading the virtual id through `rollupOptions.input`
 *     bypasses that; the plugin's `resolveId` hook sees the literal
 *     specifier and routes it to our virtual-module loader.
 */
export function buildSsrInlineConfig(opts: {
  root: string;
  ssrEntry: string;
  outDir: string;
  ssrPlugins: unknown[];
}): Record<string, unknown> {
  // If ssrEntry is a virtual id (`virtual:…`), pass it via rollupOptions.input
  // so Vite doesn't `path.resolve()` it into nonsense. For real file paths
  // we keep the historical `build.ssr: <path>` form.
  const isVirtual = opts.ssrEntry.startsWith("virtual:");
  return {
    root: opts.root,
    configFile: false,
    plugins: opts.ssrPlugins,
    // Vite would otherwise copy `<root>/public/*` into the SSR outDir.
    // We don't want public files cataloged as worker modules — they're
    // static assets. The client build keeps publicDir.
    publicDir: false,
    ssr: {
      noExternal: true,
      target: "webworker",
    },
    // Preserve `process.env.X` and `process.env` references at runtime
    // — the zeroship runtime injects a real `process.env` (populated
    // from `worker_env` and the app's exposed-secrets list). Without
    // these defines, the `target: "webworker"` SSR build statically
    // rewrites `process.env` to `{}`, so libraries like `@ai-sdk/openai`
    // that read `process.env.OPENAI_API_KEY` at runtime see undefined
    // even when the var is set on the host.
    define: {
      "process.env": "process.env",
      "process.env.NODE_ENV": '"production"',
    },
    build: {
      // `true` (instead of a path string) tells Vite "this is an SSR
      // build" without specifying the entry — entry comes from
      // rollupOptions.input below.
      ssr: isVirtual ? true : opts.ssrEntry,
      outDir: opts.outDir,
      emptyOutDir: false,
      rolldownOptions: {
        ...(isVirtual ? { input: { index: opts.ssrEntry } } : {}),
        output: { format: "esm", entryFileNames: "index.js" },
      },
      minify: true,
    },
    logLevel: "warn",
  };
}

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
    buildEnd(error) {
      if (!error && onDependencies) {
        onDependencies([...new Set([...this.getModuleIds()]
          .map(id => id.split("?")[0])
          .filter(id => isAbsolute(id)))].sort());
      }
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

/** Build server modules for retained local replay (the dev archive). */
export async function buildServerBundle(opts: {
  root: string;
  entry: string;
  outDir: string;
  clientDistDir: string;
  state: TransformState;
}): Promise<string[]> {
  let dependencies: string[] = [];
  const graph = serverGraphPlugin(ids => {
    dependencies = ids;
  });
  // Discover procedure bindings before generating the static server entry.
  // Server-only builds and retained dev archives may have no client graph
  // to populate this state before the synthetic entry loads.
  const userEntryRel = opts.entry.replace(/\\/g, "/");
  const discoveryConfig = buildSsrInlineConfig({
    root: opts.root,
    ssrEntry: userEntryRel,
    outDir: opts.outDir,
    ssrPlugins: [
      nodeCompatPlugin(),
      zeroshipModulePlugin(),
      transformPlugin(opts.state),
      clientManifestPlugin({ root: opts.root, distDir: opts.clientDistDir }),
    ],
  });
  (discoveryConfig.build as Record<string, unknown>).write = false;
  await viteBuild(discoveryConfig as Parameters<typeof viteBuild>[0]);
  const config = buildSsrInlineConfig({
    root: opts.root,
    ssrEntry: SERVER_ENTRY_VIRTUAL_ID,
    outDir: opts.outDir,
    ssrPlugins: [
      nodeCompatPlugin(),
      zeroshipModulePlugin(),
      transformPlugin(opts.state),
      rpcRegistryPlugin({
        root: opts.root,
        userEntryRel,
        state: opts.state,
      }),
      clientManifestPlugin({ root: opts.root, distDir: opts.clientDistDir }),
      graph,
    ],
  });
  await viteBuild(config as Parameters<typeof viteBuild>[0]);
  const entryPath = resolve(opts.outDir, "index.js");
  writeFileSync(entryPath, stripUseServer(readFileSync(entryPath, "utf8")), "utf8");
  return dependencies;
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
 * Resolution, platform, chunking and `process.env` handling are the values
 * Vite gives an `ssr` environment built with `ssr.target: "webworker"` and
 * `noExternal: true`, which is how the worker was built before it moved into
 * the app builder.
 */
export function applyWorkerBuildOptions(environment: EnvironmentOptions, clientOutDir: string): void {
  environment.consumer = "server";
  environment.resolve = {
    ...environment.resolve,
    conditions: [...defaultClientConditions],
    mainFields: [...defaultClientMainFields],
    builtins: [],
    noExternal: true,
  };
  // The runtime supplies a real `process.env` (from `worker_env` and the
  // app's exposed secrets), so references to it stay live. `NODE_ENV` is
  // replaced statically, which is what removes development branches.
  environment.keepProcessEnv = false;
  environment.define = {
    ...environment.define,
    "process.env": "process.env",
    "process.env.NODE_ENV": JSON.stringify("production"),
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
    target: "baseline-widely-available",
    ssr: true,
    rolldownOptions: {
      ...inherited,
      input: { index: SERVER_ENTRY_VIRTUAL_ID },
      // Rolldown's `node` platform imports `node:module` for its CommonJS
      // helpers, which the runtime does not provide.
      platform: "browser",
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
 */
async function buildWorker(builder: ViteBuilder, entry: string): Promise<void> {
  const environment = builder.environments[ZEROSHIP_ENVIRONMENT];
  if (environment == null) {
    throw new Error(
      `[zeroship] the app builder has no "${ZEROSHIP_ENVIRONMENT}" environment to build the worker in`,
    );
  }
  const discovery = new BuildEnvironment(ZEROSHIP_ENVIRONMENT, environment.getTopLevelConfig(), {
    options: { build: { write: false, rolldownOptions: { input: { index: entry } } } },
  });
  await discovery.init();
  await builder.build(discovery);
  await builder.build(environment);
  const { root, build } = environment.config;
  const entryPath = resolve(root, build.outDir, "index.js");
  writeFileSync(entryPath, stripUseServer(readFileSync(entryPath, "utf8")), "utf8");
}

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
        await genTypesFromMigrations(migrationsAbs, outDir, {
          label: database.label,
          primary: database.primary,
          check: isProd,
        });
        console.log(
          isProd
            ? `[zeroship] gen-types --check: ${database.label} env.db.ts + schema.runtime.json track the migrations`
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

  const orchestrator: Plugin = {
    name: "zeroship:build",
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
    workerBuildOnly(serverGraphPlugin()),
  ];
}
