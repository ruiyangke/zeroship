import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, test } from "vitest";
import { Processes } from "./fixture/processes";

async function fixture(run: (processes: Processes, directory: string) => Promise<void>) {
  const directory = await mkdtemp(join(tmpdir(), "kv-process-test-"));
  const processes = new Processes(directory);
  try { await run(processes, directory); }
  finally {
    await processes.close();
    await rm(directory, { recursive: true, force: true });
  }
}

async function alive(pid: number): Promise<boolean> {
  try {
    const stat = await readFile(`/proc/${pid}/stat`, "utf8");
    return stat.slice(stat.lastIndexOf(")") + 2).split(" ")[0] !== "Z";
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

test("cleanup stops owned descendants and leaves a separate service alive", async () => {
  await fixture(async (owned, directory) => {
    const separate = new Processes(directory);
    try {
      const control = separate.start("control", process.execPath, ["-e", "console.log('ready'); setInterval(() => {}, 1000)"], directory);
      const parent = owned.start("parent", process.execPath, ["-e", `
        const { spawn } = require('node:child_process');
        const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
        console.log(child.pid);
        setInterval(() => {}, 1000);
      `], directory);
      await expect.poll(() => control.output()).toContain("ready");
      await expect.poll(() => /^\d+\s*$/.test(parent.output())).toBe(true);
      const descendant = Number(parent.output());
      expect(await alive(descendant)).toBe(true);
      await owned.close();
      await expect.poll(() => alive(descendant)).toBe(false);
      expect(await alive(control.child.pid!)).toBe(true);
      separate.assertAlive();
    } finally { await separate.close(); }
  });
});

test("command failures and timeouts reject instead of continuing setup", async () => {
  await fixture(async (processes, directory) => {
    await expect(processes.run("failure", process.execPath, ["-e", "console.error('fixture failure'); process.exit(1)"], directory)).rejects.toThrow("fixture failure");
    await expect(processes.run("missing", join(directory, "missing-binary"), [], directory)).rejects.toThrow("Command failed");
    await expect(processes.run("timeout", process.execPath, ["-e", "setInterval(() => {}, 1000)"], directory, {}, 500)).rejects.toThrow("Command timed out");
    processes.assertAlive();
  });
});

test("cancellation terminates an active command and refuses further setup", async () => {
  await fixture(async (processes, directory) => {
    const run = processes.run("cancel", process.execPath, ["-e", "console.log('ready'); setInterval(() => {}, 1000)"], directory);
    const rejected = expect(run).rejects.toThrow("Dashboard fixture cancelled");
    await expect.poll(() => readFile(join(directory, "cancel.log"), "utf8")).toContain("ready");
    processes.cancel();
    await rejected;
    expect(() => processes.start("after", process.execPath, [], directory)).toThrow("Dashboard fixture cancelled");
  });
});
