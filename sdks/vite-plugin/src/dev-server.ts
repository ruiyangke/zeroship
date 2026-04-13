/**
 * Dev server — runs a zeroship runtime alongside Vite for local development.
 *
 * In dev mode:
 * - Vite serves the client on :5173 (default)
 * - zeroship runtime serves the API on :3001
 * - Vite proxy routes /_rpc to :3001/rpc
 * - File changes restart the zeroship process
 */

import { ChildProcess, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";

let serverProcess: ChildProcess | null = null;

export interface DevServerOptions {
  root: string;
  port?: number;
  serverEntry?: string;
}

/** Start the zeroship dev server as a child process */
export function startDevServer(options: DevServerOptions): void {
  const { root, port = 3001 } = options;

  // Find entry
  const entry = options.serverEntry || detectEntry(root);
  if (!entry) {
    console.log("[zeroship] No server entry found — running client-only");
    return;
  }

  // Find zeroship binary
  const bin = findBin(root);
  if (!bin) {
    console.warn("[zeroship] zeroship CLI not found — server functions won't work in dev");
    return;
  }

  stopDevServer();

  console.log(`[zeroship] Starting dev server on :${port}`);

  serverProcess = spawn(bin, ["serve", entry, `--port=${port}`, "--workers=1"], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
  });

  serverProcess.stdout?.on("data", (data: Buffer) => {
    const msg = data.toString().trim();
    if (msg) console.log(`[zeroship:server] ${msg}`);
  });

  serverProcess.stderr?.on("data", (data: Buffer) => {
    const msg = data.toString().trim();
    if (msg) console.log(`[zeroship:server] ${msg}`);
  });

  serverProcess.on("exit", (code) => {
    if (code !== null && code !== 0) {
      console.error(`[zeroship] Server exited with code ${code}`);
    }
    serverProcess = null;
  });
}

/** Restart the dev server (e.g., on file change) */
export function restartDevServer(options: DevServerOptions): void {
  stopDevServer();
  // Small delay to let the port free up
  setTimeout(() => startDevServer(options), 200);
}

/** Stop the dev server */
export function stopDevServer(): void {
  if (serverProcess) {
    serverProcess.kill("SIGTERM");
    serverProcess = null;
  }
}

/** Check if the dev server is running */
export function isDevServerRunning(): boolean {
  return serverProcess !== null && !serverProcess.killed;
}

function detectEntry(root: string): string | null {
  const candidates = [
    "src/index.ts", "src/index.tsx", "src/server.ts",
    "src/index.js", "src/server.js",
  ];
  for (const c of candidates) {
    if (existsSync(join(root, c))) return c;
  }
  return null;
}

function findBin(root: string): string | null {
  const local = join(root, "node_modules", ".bin", "zeroship");
  if (existsSync(local)) return local;
  try {
    const { execSync } = require("node:child_process");
    execSync("which zeroship", { stdio: "pipe" });
    return "zeroship";
  } catch {
    return null;
  }
}
