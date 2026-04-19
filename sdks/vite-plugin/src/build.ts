import { type Plugin, build as viteBuild } from "vite";
import { resolve, relative } from "node:path";
import { existsSync } from "node:fs";
import { transformPlugin, type TransformState } from "./transform.js";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";

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
        plugins: [transformPlugin(DEFAULT_RPC_ENDPOINT, state)],
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

      const totalFns = [...serverFunctionMap.values()].reduce((sum, fns) => sum + fns.size, 0);
      console.log(
        `[zeroship] server bundle complete — ${serverFunctionMap.size} modules, ${totalFns} server functions`
      );
    },
  };
}
