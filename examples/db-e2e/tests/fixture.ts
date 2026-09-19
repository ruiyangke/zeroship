import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { closeSync, openSync, readFileSync } from "node:fs";
import { cp, mkdir, mkdtemp, readdir, rm, symlink } from "node:fs/promises";
import { createServer } from "node:net";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import type { TestProject } from "vitest/node";

const example = fileURLToPath(new URL("..", import.meta.url));
const root = resolve(example, "../..");

export default async function setup(project: TestProject) {
  await mkdir(join(example, "tests/.artifacts"), { recursive: true });
  // Stay inside the pnpm workspace so Vite can load linked SDK modules.
  const work = await mkdtemp(join(example, "tests/.artifacts/work-"));
  const logs = await mkdtemp(join(example, "tests/.artifacts/run-"));
  const children = new Set<{ child: ChildProcess; done: Promise<number | null>; log: string }>();
  const reservations = new Set<ReturnType<typeof createServer>>();
  const abort = new AbortController();
  let cleanup: Promise<void> | undefined;
  const stop = () => cleanup ??= (async () => {
    abort.abort();
    for (const reservation of reservations) reservation.close();
    for (const { child } of children) kill(child);
    await Promise.all([...children].map(({ done }) => done));
    process.removeListener("SIGINT", interrupted);
    process.removeListener("SIGTERM", interrupted);
    await rm(work, { recursive: true, force: true });
  })();
  const interrupted = () => { process.exitCode = 130; void stop(); };
  process.once("SIGINT", interrupted);
  process.once("SIGTERM", interrupted);

  function kill(child: ChildProcess) {
    if (!child.pid) return;
    try { process.kill(-child.pid, "SIGKILL"); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; }
  }

  function start(binary: string, args: string[], directory: string, name: string, env: NodeJS.ProcessEnv) {
    abort.signal.throwIfAborted();
    const log = join(logs, `${name}.log`);
    const output = openSync(log, "w", 0o600);
    let child: ChildProcess;
    try { child = spawn(binary, args, { cwd: directory, env, detached: true, stdio: ["ignore", output, output] }); }
    finally { closeSync(output); }
    const done = new Promise<number | null>((resolve) => {
      child.once("error", (error) => { console.error(error); resolve(null); });
      child.once("exit", resolve);
    });
    const managed = { child, done, log };
    children.add(managed);
    return managed;
  }

  const cleanEnv: NodeJS.ProcessEnv = { PATH: process.env.PATH, LD_LIBRARY_PATH: process.env.LD_LIBRARY_PATH };
  async function run(binary: string, args: string[], directory: string, name: string, env = cleanEnv) {
    const managed = start(binary, args, directory, name, env);
    const timer = new AbortController();
    try {
      const code = await Promise.race([
        managed.done,
        sleep(1_200_000, undefined, { signal: timer.signal }).then(() => { throw new Error(`Command timed out: ${managed.log}`); }),
      ]);
      abort.signal.throwIfAborted();
      assert.equal(code, 0, `${managed.log}\n${readFileSync(managed.log, "utf8")}`);
      return readFileSync(managed.log, "utf8");
    } finally {
      timer.abort();
      kill(managed.child);
      await managed.done;
      children.delete(managed);
    }
  }

  async function port() {
    const server = createServer();
    reservations.add(server);
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
    const address = server.address();
    assert(address && typeof address !== "string");
    return { value: address.port, release: () => new Promise<void>((resolve, reject) => server.close((error) => {
      reservations.delete(server); if (error) reject(error); else resolve();
    })) };
  }

  function alive() {
    abort.signal.throwIfAborted();
    for (const { child, log } of children) {
      assert(child.pid && child.exitCode === null && child.signalCode === null, `Runtime exited: ${log}\n${readFileSync(log, "utf8")}`);
    }
  }

  try {
    console.info(`DB example: build SDKs and runtime; logs at ${logs}`);
    await run("pnpm", ["build"], root, "packages", process.env);
    const artifacts = await run("cargo", ["build", "-p", "zeroship-cli", "--message-format=json"], root, "cargo", process.env);
    let binary: string | undefined;
    for (const line of artifacts.split("\n").filter((line) => line.startsWith("{"))) {
      const value = JSON.parse(line);
      if (value.reason === "compiler-artifact" && value.target.name === "zeroship" && value.executable) binary = value.executable;
    }
    assert(binary, "Cargo must build the real CLI");
    const app = join(work, "app");
    await mkdir(app);
    for (const name of await readdir(example)) {
      if (!["node_modules", "dist", ".zeroship", "tests", "e2e"].includes(name)) {
        await cp(join(example, name), join(app, name), { recursive: true });
      }
    }
    await symlink(join(example, "node_modules"), join(app, "node_modules"));
    const vite = join(app, "node_modules/vite/bin/vite.js");
    await run(process.execPath, [vite, "build"], app, "build");
    await run(process.execPath, [join(root, "packages/vite-plugin/dist/cli/migrate-dev.js")], app, "migrate");
    const apiPort = await port();
    const uiPort = await port();
    const apiUrl = `http://127.0.0.1:${apiPort.value}`;
    const uiUrl = `http://127.0.0.1:${uiPort.value}`;
    await apiPort.release();
    await uiPort.release();
    start(process.execPath, [vite, "--host", "127.0.0.1", "--port", String(uiPort.value), "--strictPort"], app, "dev", {
      ...cleanEnv, DB_E2E_API_PORT: String(apiPort.value), ZEROSHIP_BIN: binary,
    });
    const deadline = Date.now() + 90_000;
    let ready = false;
    let reason: unknown;
    while (Date.now() < deadline) {
      alive();
      try {
        const response = await fetch(`${apiUrl}/__zeroship/v1/db-e2e.health`, {
          method: "POST", headers: { "content-type": "application/json" }, body: '{"json":{}}', signal: AbortSignal.timeout(5_000),
        });
        const body = await response.json() as { json?: { ok?: boolean } };
        if (response.ok && body.json?.ok) { ready = true; break; }
        reason = body;
      } catch (error) { reason = error; }
      await sleep(100, undefined, { signal: abort.signal });
    }
    assert(ready, `Runtime did not become ready: ${String(reason)}; logs at ${logs}`);
    project.provide("dbExample", { apiUrl, uiUrl, logs });
    return async () => { try { alive(); } finally { await stop(); } };
  } catch (error) {
    await stop();
    throw error;
  }
}
