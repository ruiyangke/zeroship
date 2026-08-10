// Boot a demo's REAL dev server and wait until it can actually serve.
//
// No stubs. This runs the same `pnpm dev` a creator runs: vite on the demo's
// vite port, and the vite-plugin spawning `zeroship serve` on the demo's API
// port, with vite proxying /__zeroship/* through to it.
//
// Readiness is deliberately stricter than "the port accepts a connection":
//
//   1. GET / on the vite port returns 2xx (the SPA shell exists), AND
//   2. the dev runtime answered at least once through the proxy.
//
// and the whole time we watch the child's output for the crash-restart banner
// the plugin prints ("runtime exited unexpectedly ... restarting"). A
// crash-looping runtime still serves the vite shell perfectly, so check 1 alone
// would call a dead app healthy — that is exactly the failure mode this tier
// exists to catch.

import { spawn, type ChildProcess } from "node:child_process";
import { mkdirSync, rmSync } from "node:fs";
import { resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { demoDir, missingRequirements, REPO_ROOT, type Demo } from "./demos.js";

const BOOT_TIMEOUT_MS = Number(process.env.ZEROSHIP_E2E_BOOT_TIMEOUT_MS ?? 90_000);
const CRASH_LOOP_THRESHOLD = 3;

export interface RunningDemo {
  readonly demo: Demo;
  /** Origin the browser should visit, e.g. http://localhost:5310 */
  readonly baseUrl: string;
  /** Direct origin of the zeroship dev runtime (bypasses the vite proxy). */
  readonly apiUrl: string;
  /** Everything the dev server has printed so far. */
  log(): string;
  stop(): Promise<void>;
}

export class DemoBootError extends Error {
  constructor(
    readonly demoName: string,
    readonly reason: string,
    readonly logTail: string,
  ) {
    super(
      `demo "${demoName}" dev server did not come up: ${reason}\n` +
        `--- last dev-server output ---\n${logTail}\n------------------------------`,
    );
    this.name = "DemoBootError";
  }
}

function stateDirFor(demo: Demo): string {
  const base =
    process.env.ZEROSHIP_E2E_STATE_DIR ??
    resolve(REPO_ROOT, "tests/e2e-browser/.state");
  return resolve(base, demo.name);
}

async function httpGetStatus(url: string, timeoutMs = 2_000): Promise<number | null> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(url, { signal: ctrl.signal });
    // Drain so the socket is released promptly.
    await res.arrayBuffer().catch(() => {});
    return res.status;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/** Does the dev runtime answer at all? Any HTTP status counts — a 404 from the
 *  dispatcher still proves the V8 runtime is up and routing. A dead/crash-looping
 *  runtime gives us `null` (connection refused). */
async function runtimeAnswers(apiUrl: string): Promise<boolean> {
  const status = await httpGetStatus(`${apiUrl}/__zeroship/v1/__e2e_readiness_probe__`);
  return status !== null;
}

/**
 * Start the demo. Rejects with `DemoBootError` (carrying the dev-server log
 * tail) rather than hanging or half-starting.
 */
export async function startDemo(demo: Demo): Promise<RunningDemo> {
  const missing = missingRequirements(demo);
  if (missing.length > 0) {
    throw new DemoBootError(
      demo.name,
      missing.map((m) => `${m} unset`).join(", "),
      "(dev server was never started — required environment is missing)",
    );
  }

  const cwd = demoDir(demo);
  const stateDir = stateDirFor(demo);
  rmSync(stateDir, { recursive: true, force: true });
  mkdirSync(stateDir, { recursive: true });

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    // Unique dev-runtime port for this demo (read by its vite.config.ts).
    [demo.apiPortEnv]: String(demo.apiPort),
    // Private per-run state: the redb file takes an EXCLUSIVE lock, so without
    // this two runs of the same demo crash-loop no matter what the ports are.
    ZEROSHIP_KV_PATH: resolve(stateDir, "kv.redb"),
    // A fresh database per run is what makes "reload and it is still there"
    // meaningful rather than a read of last run's leftovers. Demos that declare
    // a DATABASE_URL requirement keep the caller's value.
    ...(demo.requires.includes("DATABASE_URL")
      ? {}
      : { DATABASE_URL: `sqlite:${resolve(stateDir, "dev.sqlite")}` }),
    ZEROSHIP_BIN: process.env.ZEROSHIP_BIN ?? resolve(REPO_ROOT, "target/release/zeroship"),
    // Keep vite from stealing a neighbouring port when ours is taken: we want a
    // loud bind failure, not a silent move to a port nobody is watching.
    FORCE_COLOR: "0",
  };

  const child: ChildProcess = spawn(
    "pnpm",
    ["dev", "--port", String(demo.vitePort), "--strictPort"],
    { cwd, env, stdio: ["ignore", "pipe", "pipe"], detached: true },
  );

  interface ExitInfo {
    code: number | null;
    signal: NodeJS.Signals | null;
  }

  let output = "";
  let crashRestarts = 0;
  // Written from the 'exit' handler. Read through `exitInfo()` so TypeScript
  // does not narrow it to `never` inside the poll loop below (it cannot see
  // that the callback runs between iterations).
  let exited: ExitInfo | null = null;
  const exitInfo = (): ExitInfo | null => exited;

  const absorb = (chunk: Buffer) => {
    const text = chunk.toString();
    output += text;
    // The vite-plugin prints this each time the spawned `zeroship serve` dies.
    // Repeated occurrences mean the runtime cannot stay up.
    const hits = text.split("runtime exited unexpectedly").length - 1;
    crashRestarts += hits;
  };
  child.stdout?.on("data", absorb);
  child.stderr?.on("data", absorb);
  child.on("exit", (code, signal) => {
    exited = { code, signal };
  });

  const logTail = (lines = 60) => output.split("\n").slice(-lines).join("\n");

  const stop = async (): Promise<void> => {
    if (child.pid === undefined) return;
    try {
      // `pnpm dev` -> vite -> `zeroship serve`: kill the whole group, otherwise
      // the runtime survives, keeps the port, and keeps the redb lock.
      process.kill(-child.pid, "SIGTERM");
    } catch {
      /* already gone */
    }
    for (let i = 0; i < 50 && exitInfo() === null; i++) await sleep(100);
    if (exitInfo() === null) {
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch {
        /* already gone */
      }
    }
  };

  const deadline = Date.now() + BOOT_TIMEOUT_MS;
  try {
    let sawShell = false;
    while (Date.now() < deadline) {
      const dead = exitInfo();
      if (dead !== null) {
        throw new DemoBootError(
          demo.name,
          `dev server process exited early (code=${dead.code}, signal=${dead.signal})`,
          logTail(),
        );
      }
      if (crashRestarts >= CRASH_LOOP_THRESHOLD) {
        throw new DemoBootError(
          demo.name,
          `the zeroship dev runtime crash-looped (${crashRestarts} restarts). ` +
            "The vite shell may still serve HTTP 200 while this is happening, so " +
            "a page-loads check would call this app healthy.",
          logTail(),
        );
      }

      if (!sawShell) {
        const status = await httpGetStatus(`http://localhost:${demo.vitePort}/`);
        sawShell = status !== null && status >= 200 && status < 300;
      }
      if (sawShell && (await runtimeAnswers(`http://localhost:${demo.apiPort}`))) {
        return {
          demo,
          baseUrl: `http://localhost:${demo.vitePort}`,
          apiUrl: `http://localhost:${demo.apiPort}`,
          log: () => output,
          stop,
        };
      }
      await sleep(400);
    }

    throw new DemoBootError(
      demo.name,
      sawShell
        ? `the vite shell served on :${demo.vitePort} but the zeroship dev runtime never ` +
          `answered on :${demo.apiPort} within ${BOOT_TIMEOUT_MS}ms`
        : `nothing served on http://localhost:${demo.vitePort} within ${BOOT_TIMEOUT_MS}ms`,
      logTail(),
    );
  } catch (err) {
    await stop();
    throw err;
  }
}
