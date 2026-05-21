import { type Plugin, build as viteBuild } from "vite";
import { resolve, relative } from "node:path";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { transformPlugin, type TransformState } from "./transform.js";
import { nodeCompatPlugin } from "./node-compat.js";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";
import { emitZship } from "./zship.js";
import {
  rpcRegistryPlugin,
  SERVER_ENTRY_VIRTUAL_ID,
} from "./rpc-registry.js";
import {
  computeManifestExtras,
  type DiscoveredProcedure,
} from "./manifest.js";

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
 * Build the InlineConfig the plugin passes to `viteBuild()` for the
 * SSR sub-build.
 *
 * Exposed (and exported) so tests can verify the shape without spinning
 * up a real Vite environment. Notable invariants:
 *   - `publicDir: false` — the SSR outDir is `dist/server/`, and Vite's
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
 * virtual ESM module to the SSR bundle. Resolved at the SSR build's
 * `load` time, which happens AFTER the client build has written
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
        // Client build hasn't run yet (e.g., SSR-only build, dev, or
        // the build runs before the client's writeBundle finished).
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
 * module evaluates (see `crates/runtime/src/embed/node-globals.js`),
 * so the vite-plugin no longer needs to prepend a prelude.
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
 * Stage 5b — the new synthetic entry is a normaliser that always emits
 * `fetch:` on default. This probe used to just check for any
 * `export default`; now it tries to detect whether the user actually
 * wrote a `fetch` handler. Heuristics (kept simple — full AST analysis
 * would over-fit):
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
  // somewhere inside the object literal. We don't try to brace-match
  // perfectly; the false-positive cost (Worker(SSR) instead of Static
  // when the user has a NESTED `fetch` key but no top-level one) is
  // negligible.
  const defaultBlock = stripped.match(/export\s+default\s+(\{[\s\S]*\})/);
  if (defaultBlock) {
    const block = defaultBlock[1];
    // Top-level keys: `fetch:` (property), `fetch(` (method shorthand),
    // `fetch,` / `fetch}` (shorthand from a binding).
    if (/(?:^|[,{\s])fetch\s*[:(,}]/.test(block)) return true;
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

export function buildPlugin(
  state: TransformState,
  options: {
    serverEntry?: string;
    /** "static" → skip the SSR sub-build entirely. */
    mode?: "full" | "static";
  } = {}
): Plugin {
  const { serverFunctionMap } = state;
  const mode = options.mode ?? "full";
  let root = "";
  let isDev = false;
  // The client build's `outDir` (resolved). Read in configResolved so the
  // closeBundle hook knows where to look for `.zship` inputs.
  let clientOutDir = "";
  // Whether we already ran the server SSR build for this Vite invocation.
  // closeBundle fires once per environment per invocation; without this
  // guard we'd re-emit the same archive (or worse, re-trigger the SSR
  // build inside its own closeBundle).
  let serverBuilt = false;
  let zshipEmitted = false;
  // Whether the user's SSR entry source contains `export default`.
  // Probed before Rollup runs so it isn't confused by the bootstrap's
  // own appended default. Conservative default = true (emit Worker(SSR)
  // catch-all when in doubt; better to 404 than serve stale shell).
  let userHasDefaultFetch = true;
  // Vite's resolved mode — drives the manifest emitter's
  // production-mode gate (every procedure must have an explicit `id`
  // when shipping a production build).
  let viteMode: "production" | "development" = "production";

  return {
    name: "zeroship:build",

    /**
     * In static mode, satisfy Vite's "needs at least one input" check
     * by injecting a virtual entry that resolves to an empty module.
     * The matching `generateBundle` below deletes the empty chunk so
     * the .zship emitter doesn't catalog a `_empty-<hash>.js` asset.
     *
     * Only runs when the user hasn't already configured an input; if
     * they have an `index.html` or other entry, we leave it alone
     * and the static-mode build still produces the JS chunks for it
     * (see test `static_only_with_html_inputs_still_works`).
     */
    config(userConfig: any) {
      if (mode !== "static") return;
      const hasInput =
        userConfig?.build?.rollupOptions?.input != null ||
        // Vite auto-uses `index.html` in the project root if it exists.
        // We can't check that here without the resolved root, so just
        // err on the side of injecting our virtual entry only when the
        // user explicitly has no rollupOptions.input. Vite falls back
        // to its default for the index.html case.
        false;
      if (hasInput) return;
      return {
        build: {
          rollupOptions: {
            input: { __zeroship_static_stub: STATIC_STUB_VIRTUAL_ID },
          },
        },
      };
    },

    /**
     * Provide the virtual stub module the `config` hook injected above.
     * Resolves to an empty ESM module — Rollup emits a chunk for it
     * which `generateBundle` then deletes.
     */
    resolveId(id: string) {
      if (mode !== "static") return null;
      if (id === STATIC_STUB_VIRTUAL_ID) return STATIC_STUB_RESOLVED_ID;
      return null;
    },
    load(id: string) {
      if (mode !== "static") return null;
      if (id !== STATIC_STUB_RESOLVED_ID) return null;
      // Empty module — no exports, no side effects.
      return "// zeroship static-mode stub\n";
    },
    /**
     * After Rollup builds the (empty) stub chunk, delete its output so
     * the .zship doesn't end up shipping a `_empty-<hash>.js`.
     */
    generateBundle(_options: unknown, bundle: Record<string, { name?: string }>) {
      if (mode !== "static") return;
      for (const [filename, chunk] of Object.entries(bundle)) {
        if (chunk.name === "__zeroship_static_stub") {
          delete bundle[filename];
        }
      }
    },

    configResolved(config: any) {
      root = config.root;
      isDev = config.command === "serve";
      // Vite's ResolvedConfig.mode reflects the `--mode` flag
      // (`production` for `vite build` by default; `development` for
      // `vite build --mode development`). Anything other than the two
      // recognized values falls back to `production` for the gate —
      // custom modes (e.g. "staging") are still real ships.
      viteMode = config.mode === "development" ? "development" : "production";
      // Resolve the client outDir relative to root. By default this is
      // `dist`; user can override via `build.outDir`.
      const buildOutDir = config.build?.outDir ?? "dist";
      clientOutDir = resolve(root, buildOutDir);
    },

    async writeBundle() {
      if (isDev) return;
      if (serverBuilt) return;
      // Static-mode: skip the SSR sub-build entirely. closeBundle still
      // runs the emitter, which walks dist/ as-is.
      if (mode === "static") {
        serverBuilt = true;
        return;
      }

      const entry = findServerEntry(root, options.serverEntry);
      if (!entry) {
        console.warn("[zeroship] no server entry found — skipping server bundle");
        // We still want to emit a static-only .zship in this case,
        // so the closeBundle hook handles the SSG path.
        return;
      }
      serverBuilt = true;

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

      // Build via the synthetic server entry (`virtual:zeroship/_server-entry`)
      // — that virtual module imports the user's entry, re-exports its
      // bindings, and provides our own `default.{ fetch, rpc }`.
      // Rolldown collapses everything into a single ESM file at
      // `dist/server/index.js`. The synthetic entry's `_procedures`
      // dispatch table is built at module-init time by iterating the
      // user namespace's exports — no global registry, no side effects.
      //
      // Use the user's absolute entry path as the synthetic entry's
      // import specifier — virtual modules have no parent path, so
      // relative specifiers don't anchor to anything sensible.
      const userEntryRel = entry.replace(/\\/g, "/");

      // Stage 5c: schema discovery runs in the runtime bootstrap and
      // reads `user.default.schema` directly off the loaded entry. The
      // synthetic entry already re-exports `_zsUserDefault.schema` on
      // its own `default.schema` (Stage 5b normaliser). No
      // `manifest.exports.schema` field is written; no resolver runs.

      const ssrConfig = buildSsrInlineConfig({
        root,
        ssrEntry: SERVER_ENTRY_VIRTUAL_ID,
        outDir: "dist/server",
        ssrPlugins: [
          // node-compat MUST come first so its `resolve.id` returns
          // the polyfill path before Vite tries to load `node:crypto`
          // etc. as bare specifiers.
          nodeCompatPlugin(),
          // transformPlugin rewrites server modules with SSR-hook
          // patches (.useQuery / .prefetch / etc.) and records procedure
          // metadata into `state.discoveredProcedures` for the manifest
          // emitter. It does NOT inject any registry side-effects any
          // more — the synthetic SSR entry discovers procedures at
          // module-init time from the user namespace's exports.
          transformPlugin(DEFAULT_RPC_ENDPOINT, state),
          // Synthetic SSR entry virtual module owner. The entry's body
          // is build-time-static and order-independent.
          rpcRegistryPlugin({
            root,
            userEntryRel,
            state,
          }),
          // Expose `virtual:zeroship/client-manifest` so SSR code can
          // read hashed asset paths at build time. The client build's
          // `writeBundle` (this hook) finishes BEFORE we kick off the
          // SSR build, so `dist/.vite/manifest.json` is already on disk.
          clientManifestPlugin({ root, distDir: relative(root, clientOutDir) }),
        ],
      });
      // Cast — buildSsrInlineConfig returns a record so it can be
      // tested without importing Vite types into the test runner.
      await viteBuild(ssrConfig as Parameters<typeof viteBuild>[0]);

      // Strip the leading `"use server"` directive (a bare string
      // expression that is a no-op but pollutes the output). Node-shaped
      // globals (process, Buffer, setImmediate, etc.) are installed by
      // the Rust runtime on every isolate before any user module
      // evaluates — see `crates/runtime/src/embed/node-globals.js`.
      const bundlePath = resolve(root, "dist/server/index.js");
      try {
        const stripped = stripUseServer(readFileSync(bundlePath, "utf8"));
        writeFileSync(bundlePath, stripped, "utf8");
      } catch (e) {
        console.warn(`[zeroship] failed to strip use-server directive: ${(e as Error).message}`);
      }

      const totalFns = [...serverFunctionMap.values()].reduce((sum, fns) => sum + fns.size, 0);
      console.log(
        `[zeroship] server bundle complete — ${serverFunctionMap.size} modules, ${totalFns} server functions`
      );
    },

    /**
     * After both client and server builds have written their bundles,
     * emit the `.zship` archive. closeBundle fires at the very end
     * of the Vite build lifecycle — once for this plugin instance per
     * `vite build` invocation, regardless of how many environments
     * Vite ran. (Each environment in a multi-env build gets its own
     * plugin context, but our plugin is registered on the client
     * environment only; the writeBundle above kicks off the SSR build
     * via `viteBuild()`, which is a separate build that does NOT run
     * this plugin's closeBundle.)
     *
     * We use closeBundle (not writeBundle) so:
     *   1. The server SSR build has finished (writeBundle above
     *      awaits it before returning).
     *   2. All asset hashes are stable on disk.
     *   3. We're outside of any Rollup/rolldown emit path — file
     *      operations don't have to thread through Vite's emitFile.
     */
    async closeBundle() {
      if (isDev) return;
      if (zshipEmitted) return;
      zshipEmitted = true;

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

        const extras = await computeManifestExtras({
          root,
          procedures,
          mode: viteMode,
        });

        // Stage 5c: schema lives on the synthetic entry's
        // `default.schema` (passed through from the user module). The
        // runtime reads it directly at boot — no
        // `manifest.exports.schema` field, no resolver, no log line.

        await emitZship({
          root,
          distDir: clientOutDir,
          compiler: getCompilerId(),
          userHasDefaultFetch,
          rpcExtras: {
            resources: extras.resources,
            transformer: extras.transformer,
          },
        });
      } catch (e) {
        console.error(`[zeroship] failed to emit .zship: ${(e as Error).message}`);
        throw e;
      }
    },
  };
}
