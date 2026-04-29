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

      console.log(`[zeroship] building server bundle from ${relative(root, entry)}`);

      // Reuse the shared transform state so the server bundle emits
      // `__register(...)` side effects for every "use server" export
      // the client build already registered. Without this, the server
      // bundle has the plain function bodies but no registry
      // population, so the V8 runtime sees "Method not found" for
      // every URL-path RPC call.
      await viteBuild({
        root,
        configFile: false,
        plugins: [
          // node-compat MUST come first so its `resolve.id` returns
          // the polyfill path before Vite tries to load `node:crypto`
          // etc. as bare specifiers.
          nodeCompatPlugin(),
          transformPlugin(DEFAULT_RPC_ENDPOINT, state),
        ],
        ssr: {
          // Bundle every dependency into the server bundle. Without
          // this, the SSR build leaves `import "deepagents"` etc. as
          // ESM imports the V8 runtime can't resolve at boot — we
          // need a single self-contained file. `noExternal: true`
          // forces all deps to be inlined; the node-compat plugin
          // intercepts `node:*` reaches at the import-resolution
          // stage so they don't bundle node-only code.
          noExternal: true,
          target: "webworker",
        },
        build: {
          ssr: entry,
          outDir: "dist/server",
          emptyOutDir: false,
          rolldownOptions: {
            output: { format: "esm", entryFileNames: "index.js" },
          },
          minify: true,
        },
        logLevel: "warn",
      });

      // Prelude (prepended) installs Node-shaped globals + the
      // Map-backed __register registry. Bootstrap (appended) adds
      // the dispatchRpc + default.fetch exports that the runtime's
      // BOOTSTRAP_JS looks for. Without either, the worker loads the
      // bundle but logs "No default.fetch handler exported".
      const bundlePath = resolve(root, "dist/server/index.js");
      try {
        const prelude = readFileSync(PRELUDE_PATH, "utf8");
        const bootstrap = readFileSync(BOOTSTRAP_PATH, "utf8");
        const original = readFileSync(bundlePath, "utf8");
        // Drop the leading `"use server";` that the bundler emits —
        // it's a no-op directive that's just bytes after we prepend.
        const stripped = original.replace(/^"use server"\s*;\s*/, "");
        writeFileSync(bundlePath, prelude + "\n" + stripped + "\n" + bootstrap, "utf8");
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
        });
      } catch (e) {
        console.error(`[zeroship] failed to emit .zsapp: ${(e as Error).message}`);
        throw e;
      }
    },
  };
}
