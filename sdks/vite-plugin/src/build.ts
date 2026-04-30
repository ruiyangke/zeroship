import { type Plugin, build as viteBuild } from "vite";
import { resolve, relative } from "node:path";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { transformPlugin, type TransformState } from "./transform.js";
import { nodeCompatPlugin } from "./node-compat.js";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";
import { emitZsapp } from "./zsapp.js";

const HERE = resolve(fileURLToPath(import.meta.url), "..");
const PRELUDE_PATH   = resolve(HERE, "../src/runtime-prelude.js");   // prepended — Node-globals shim
const BOOTSTRAP_PATH = resolve(HERE, "../src/server-bootstrap.js");  // appended  — dispatchRpc + default.fetch

/**
 * Marker line emitted at the top of the appended server bootstrap.
 * Tests (and humans inspecting the bundle) can grep for this to tell
 * whether the bootstrap was actually attached to a given SSR bundle.
 */
export const BOOTSTRAP_MARKER = "// zeroship server bootstrap";

/** Public specifier for the client manifest virtual module. */
export const CLIENT_MANIFEST_VIRTUAL_ID = "virtual:zeroship/client-manifest";
/** Internal (\0-prefixed) id Vite uses for the same module. */
export const CLIENT_MANIFEST_RESOLVED_ID = "\0" + CLIENT_MANIFEST_VIRTUAL_ID;

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
 *   - `entryFileNames: "index.js"` — deterministic name; the .zsapp
 *     emitter uses it as the worker entry.
 */
export function buildSsrInlineConfig(opts: {
  root: string;
  ssrEntry: string;
  outDir: string;
  ssrPlugins: unknown[];
}): Record<string, unknown> {
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
    build: {
      ssr: opts.ssrEntry,
      outDir: opts.outDir,
      emptyOutDir: false,
      rolldownOptions: {
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
 * Probe a server-entry source string for `export default`.
 *
 * Source-string regex (not AST) — fast, no parser dependency, and the
 * shape we care about is unambiguous in practice:
 *   - `export default <expr>` (object, function, identifier, …)
 *   - `export default function …`
 *   - `export default class …`
 *
 * NOT detected (returns false → "user has no default"):
 *   - `export { foo as default }`  — rare, ambiguous
 *   - the alias / re-export form  — also rare in SSR entries
 *
 * Per the platform contract, `default.fetch` must come from the entry
 * file directly, so we do not chase imports.
 *
 * Default behavior on ambiguity is to return `true` (assume user has
 * default), since the conservative choice is to emit Worker(SSR) — an
 * unwanted SSR catch-all 404s, while an unwanted Static catch-all
 * serves a stale shell on intended SSR routes.
 */
export function userSourceHasDefaultExport(source: string): boolean {
  // Strip /* ... */ block comments and // line comments before probing
  // so a commented-out `export default` doesn't trip the match.
  const stripped = source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:])\/\/.*$/gm, "$1");
  // Multi-line: matches at the start of any line (after optional ws).
  return /^\s*export\s+default\b/m.test(stripped);
}

/**
 * Wrap the rolled-up SSR bundle with the prelude (always) and the
 * bootstrap (only when the user did NOT export their own default).
 *
 * Two ESM `export default` statements in the same module are a syntax
 * error, so when the user provides `default.fetch` we have to skip the
 * append. The user is then responsible for handling /_rpc/* themselves;
 * the prelude still installs the registry side-effect target (`__register`).
 *
 * TODO(mode 3): wrap-pattern that renames the user's default to
 * `__userFetch`, keeps the bootstrap's RPC dispatch, and falls through
 * to `__userFetch` for non-RPC. Not needed for the current SSR demo.
 */
export function wrapServerBundle(opts: {
  prelude: string;
  bootstrap: string;
  bundle: string;
  userHasDefaultFetch: boolean;
}): string {
  // Drop the leading `"use server";` directive — it's a no-op once
  // we prepend the prelude.
  const stripped = opts.bundle.replace(/^"use server"\s*;\s*/, "");
  const head = opts.prelude + "\n" + stripped;
  if (opts.userHasDefaultFetch) {
    // User owns default.fetch — appending the bootstrap would create
    // a duplicate `export default`, breaking the bundle.
    return head + "\n";
  }
  return head + "\n" + BOOTSTRAP_MARKER + "\n" + opts.bootstrap;
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

export function buildPlugin(state: TransformState, options: { serverEntry?: string } = {}): Plugin {
  const { serverFunctionMap } = state;
  let root = "";
  let isDev = false;
  // The client build's `outDir` (resolved). Read in configResolved so the
  // closeBundle hook knows where to look for `.zsapp` inputs.
  let clientOutDir = "";
  // Whether we already ran the server SSR build for this Vite invocation.
  // closeBundle fires once per environment per invocation; without this
  // guard we'd re-emit the same archive (or worse, re-trigger the SSR
  // build inside its own closeBundle).
  let serverBuilt = false;
  let zsappEmitted = false;
  // Whether the user's SSR entry source contains `export default`.
  // Probed before Rollup runs so it isn't confused by the bootstrap's
  // own appended default. Conservative default = true (emit Worker(SSR)
  // catch-all when in doubt; better to 404 than serve stale shell).
  let userHasDefaultFetch = true;

  return {
    name: "zeroship:build",

    configResolved(config: any) {
      root = config.root;
      isDev = config.command === "serve";
      // Resolve the client outDir relative to root. By default this is
      // `dist`; user can override via `build.outDir`.
      const buildOutDir = config.build?.outDir ?? "dist";
      clientOutDir = resolve(root, buildOutDir);
    },

    async writeBundle() {
      if (isDev) return;
      if (serverBuilt) return;

      const entry = findServerEntry(root, options.serverEntry);
      if (!entry) {
        console.warn("[zeroship] no server entry found — skipping server bundle");
        // We still want to emit a static-only .zsapp in this case,
        // so the closeBundle hook handles the SSG path.
        return;
      }
      serverBuilt = true;

      // Probe the entry source BEFORE Rollup runs and BEFORE the
      // bootstrap is appended — otherwise the bootstrap's own
      // `export default { fetch }` would always trigger the match.
      try {
        const entrySource = readFileSync(entry, "utf8");
        userHasDefaultFetch = userSourceHasDefaultExport(entrySource);
      } catch {
        // If we can't read the entry, fall back to the conservative
        // default (true → Worker(SSR) catch-all).
        userHasDefaultFetch = true;
      }

      console.log(`[zeroship] building server bundle from ${relative(root, entry)}`);

      // Reuse the shared transform state so the server bundle emits
      // `__register(...)` side effects for every "use server" export
      // the client build already registered. Without this, the server
      // bundle has the plain function bodies but no registry
      // population, so the V8 runtime sees "Method not found" for
      // every URL-path RPC call.
      const ssrConfig = buildSsrInlineConfig({
        root,
        ssrEntry: entry,
        outDir: "dist/server",
        ssrPlugins: [
          // node-compat MUST come first so its `resolve.id` returns
          // the polyfill path before Vite tries to load `node:crypto`
          // etc. as bare specifiers.
          nodeCompatPlugin(),
          // Expose `virtual:zeroship/client-manifest` so SSR code can
          // read hashed asset paths at build time. The client build's
          // `writeBundle` (this hook) finishes BEFORE we kick off the
          // SSR build, so `dist/.vite/manifest.json` is already on disk.
          clientManifestPlugin({ root, distDir: relative(root, clientOutDir) }),
          transformPlugin(DEFAULT_RPC_ENDPOINT, state),
        ],
      });
      // Cast — buildSsrInlineConfig returns a record so it can be
      // tested without importing Vite types into the test runner.
      await viteBuild(ssrConfig as Parameters<typeof viteBuild>[0]);

      // Prelude (prepended) installs Node-shaped globals + the
      // Map-backed __register registry. Bootstrap (appended) adds
      // the dispatchRpc + default.fetch exports that the runtime's
      // BOOTSTRAP_JS looks for — but ONLY if the user didn't already
      // export their own default. Two `export default` statements in
      // the same ESM module are a syntax error.
      const bundlePath = resolve(root, "dist/server/index.js");
      try {
        const wrapped = wrapServerBundle({
          prelude: readFileSync(PRELUDE_PATH, "utf8"),
          bootstrap: readFileSync(BOOTSTRAP_PATH, "utf8"),
          bundle: readFileSync(bundlePath, "utf8"),
          userHasDefaultFetch,
        });
        writeFileSync(bundlePath, wrapped, "utf8");
        if (userHasDefaultFetch) {
          console.log(
            `[zeroship] user exports default.fetch — bootstrap append skipped`
          );
        }
      } catch (e) {
        console.warn(`[zeroship] failed to wrap prelude+bootstrap: ${(e as Error).message}`);
      }

      const totalFns = [...serverFunctionMap.values()].reduce((sum, fns) => sum + fns.size, 0);
      console.log(
        `[zeroship] server bundle complete — ${serverFunctionMap.size} modules, ${totalFns} server functions`
      );
    },

    /**
     * After both client and server builds have written their bundles,
     * emit the `.zsapp` archive. closeBundle fires at the very end
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
      if (zsappEmitted) return;
      zsappEmitted = true;

      try {
        await emitZsapp({
          root,
          distDir: clientOutDir,
          compiler: getCompilerId(),
          userHasDefaultFetch,
        });
      } catch (e) {
        console.error(`[zeroship] failed to emit .zsapp: ${(e as Error).message}`);
        throw e;
      }
    },
  };
}
