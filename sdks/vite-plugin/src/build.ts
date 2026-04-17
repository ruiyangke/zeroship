import { type Plugin, build as viteBuild } from "vite";
import { resolve, relative } from "node:path";
import { existsSync } from "node:fs";
import type { TransformState } from "./transform.js";

/** Find server entry point in project */
export function findServerEntry(root: string, explicit?: string): string | null {
  if (explicit && existsSync(resolve(root, explicit))) return resolve(root, explicit);
  for (const candidate of [
    "src/server.ts",
    "src/server.js",
    "server.ts",
    "server.js",
    "src/index.server.ts",
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

      await viteBuild({
        root,
        configFile: false,
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

      const totalFns = [...serverFunctionMap.values()].reduce((sum, fns) => sum + fns.length, 0);
      console.log(
        `[zeroship] server bundle complete — ${serverFunctionMap.size} modules, ${totalFns} server functions`
      );
    },
  };
}
