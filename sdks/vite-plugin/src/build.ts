import type { Plugin } from "vite";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import type { TransformState } from "./transform.js";

/** Find server entry point in project */
export function findServerEntry(root: string, explicit?: string): string | null {
  if (explicit) return explicit;
  const candidates = ["src/index.ts", "src/index.tsx", "src/server.ts", "src/index.js", "src/server.js"];
  for (const c of candidates) {
    if (existsSync(resolve(root, c))) return c;
  }
  return null;
}

export function buildPlugin(state: TransformState): Plugin {
  const { serverFunctionMap } = state;
  let isDev = false;
  let root = "";
  let config: any;

  return {
    name: "zeroship:build",

    configResolved(resolvedConfig: any) {
      config = resolvedConfig;
      isDev = resolvedConfig.command === "serve";
      root = resolvedConfig.root;
    },

    async closeBundle() {
      if (isDev) return;

      const entry = findServerEntry(root, config?._zeroshipServerEntry);
      if (!entry) { console.log("[zeroship] No server entry — client-only build"); return; }

      const outDir = config.build?.outDir
        ? resolve(root, config.build.outDir)
        : resolve(root, "dist");

      const serverOut = resolve(outDir, "server");

      console.log("\n[zeroship] Building server bundle with Rolldown...");

      try {
        // Use esbuild for server bundle — simple, fast, no Vite recursion
        const { execSync } = await import("node:child_process" as string);
        const { mkdirSync } = await import("node:fs" as string);
        mkdirSync(serverOut, { recursive: true });
        const outFile = resolve(serverOut, "server.js");
        const entryFile = resolve(root, entry);
        const minFlag = config.build?.minify !== false ? "--minify" : "";
        execSync(
          `npx esbuild ${entryFile} --bundle --format=esm --platform=neutral --main-fields=module,main --outfile=${outFile} ${minFlag}`.trim(),
          { cwd: root, stdio: "pipe" }
        );

        console.log(`[zeroship] Server: ${outFile}`);
      } catch (e) {
        console.error("[zeroship] Server build failed:", e);
      }

      // Build report
      if (serverFunctionMap.size > 0) {
        console.log("\n[zeroship] Server/client split:");
        for (const [file, fns] of serverFunctionMap) {
          console.log(`  ${file}: ${fns.join(", ")}`);
        }
      }
    },
  };
}
