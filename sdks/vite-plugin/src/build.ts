import { type Plugin, build as viteBuild } from "vite";
import { resolve, relative } from "node:path";
import { existsSync, readFileSync, appendFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { transformPlugin, type TransformState } from "./transform.js";
import { nodeCompatPlugin } from "./node-compat.js";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";

const HERE = resolve(fileURLToPath(import.meta.url), "..");
const PRELUDE_PATH = resolve(HERE, "../src/runtime-prelude.js"); // relative to dist/build.js

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

  return {
    name: "zeroship:build",

    configResolved(config: any) {
      root = config.root;
      isDev = config.command === "serve";
    },

    async writeBundle() {
      if (isDev) return;

      const entry = findServerEntry(root, options.serverEntry);
      if (!entry) {
        console.warn("[zeroship] no server entry found — skipping server bundle");
        return;
      }

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

      // Prepend the runtime prelude so deepagents / langchain see
      // process / Buffer / global / setImmediate without first
      // having to import a node:* polyfill explicitly. The unenv
      // shims still kick in for `import { ... } from "node:..."`
      // sites; the prelude only fills the "code reads
      // globalThis.process.env without importing process" pattern.
      const bundlePath = resolve(root, "dist/server/index.js");
      try {
        const prelude = readFileSync(PRELUDE_PATH, "utf8");
        const original = readFileSync(bundlePath, "utf8");
        // Remove the leading `"use server";` from the original — the
        // bundler emitted it but it's a no-op directive that wastes
        // bytes when we prepend ahead of it.
        const stripped = original.replace(/^"use server"\s*;\s*/, "");
        writeFileSync(bundlePath, prelude + "\n" + stripped, "utf8");
      } catch (e) {
        console.warn(`[zeroship] failed to prepend prelude: ${(e as Error).message}`);
      }

      const totalFns = [...serverFunctionMap.values()].reduce((sum, fns) => sum + fns.size, 0);
      console.log(
        `[zeroship] server bundle complete — ${serverFunctionMap.size} modules, ${totalFns} server functions`
      );
    },
  };
}
