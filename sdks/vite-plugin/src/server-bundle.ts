/**
 * Server bundle collection — gathers all server code for bundling.
 *
 * During the Vite build, we collect paths to all modules that contain
 * server functions. After Vite finishes the client build, we bundle
 * the server code separately using esbuild (via the zeroship CLI).
 */

import { execSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

export interface ServerBundleOptions {
  /** Project root directory */
  root: string;
  /** Output directory for the server bundle */
  outDir: string;
  /** Server entry point (auto-detected if not specified) */
  serverEntry?: string;
  /** Whether to minify the server bundle */
  minify?: boolean;
}

/**
 * Build the server bundle using the zeroship CLI.
 *
 * Detects the server entry point (src/index.ts or src/server.ts),
 * runs `zeroship build`, and outputs the .appbundle to the outDir.
 */
export function buildServerBundle(options: ServerBundleOptions): {
  bundlePath: string;
  serverFunctions: string[];
} | null {
  const { root, outDir, minify } = options;

  // Detect server entry
  const entry = options.serverEntry || detectServerEntry(root);
  if (!entry) {
    return null; // No server code — client-only app
  }

  const serverOutDir = join(outDir, "server");
  mkdirSync(serverOutDir, { recursive: true });

  // Try zeroship CLI first, fall back to esbuild directly
  const zeroshipBin = findZeroshipBin(root);

  if (zeroshipBin) {
    try {
      const args = [
        "build",
        root,
        `--outdir=${serverOutDir}`,
        ...(minify ? ["--minify"] : []),
      ];
      execSync(`${zeroshipBin} ${args.join(" ")}`, {
        cwd: root,
        stdio: "pipe",
      });

      const bundlePath = join(serverOutDir, "app.appbundle");
      if (existsSync(bundlePath)) {
        // Read manifest for server function list
        const manifestPath = join(serverOutDir, "manifest.json");
        let serverFunctions: string[] = [];
        if (existsSync(manifestPath)) {
          const manifest = JSON.parse(readFileSync(manifestPath, "utf-8"));
          serverFunctions = manifest.server_functions || [];
        }
        return { bundlePath, serverFunctions };
      }
    } catch {
      // Fall through to esbuild
    }
  }

  // Fallback: bundle with esbuild directly
  try {
    const esbuildArgs = [
      entry,
      "--bundle",
      "--format=esm",
      "--platform=neutral",
      "--main-fields=module,main",
      `--outfile=${join(serverOutDir, "server.js")}`,
      ...(minify ? ["--minify"] : []),
    ];
    execSync(`npx esbuild ${esbuildArgs.join(" ")}`, {
      cwd: root,
      stdio: "pipe",
    });

    return {
      bundlePath: join(serverOutDir, "server.js"),
      serverFunctions: [],
    };
  } catch (e) {
    console.error("[zeroship] Failed to build server bundle:", e);
    return null;
  }
}

/** Detect the server entry point in a project */
function detectServerEntry(root: string): string | null {
  const candidates = [
    "src/index.ts",
    "src/index.tsx",
    "src/server.ts",
    "src/index.js",
    "src/server.js",
    "index.ts",
    "server.ts",
  ];
  for (const candidate of candidates) {
    if (existsSync(join(root, candidate))) {
      return candidate;
    }
  }
  return null;
}

/** Find the zeroship CLI binary */
function findZeroshipBin(root: string): string | null {
  // Check node_modules/.bin
  const localBin = join(root, "node_modules", ".bin", "zeroship");
  if (existsSync(localBin)) return localBin;

  // Check if globally installed
  try {
    execSync("which zeroship", { stdio: "pipe" });
    return "zeroship";
  } catch {
    return null;
  }
}
