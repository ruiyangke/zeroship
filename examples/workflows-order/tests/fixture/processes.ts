import { spawn, type ChildProcess } from "node:child_process";
import { closeSync, openSync, readFileSync } from "node:fs";
import { join } from "node:path";

type Exit = { code: number | null; signal: NodeJS.Signals | null; error?: Error };

export class ManagedProcess {
  readonly child: ChildProcess;
  readonly exited: Promise<Exit>;
  private exit?: Exit;
  private stopped = false;

  constructor(binary: string, args: string[], cwd: string, env: NodeJS.ProcessEnv, readonly log: string) {
    const output = openSync(log, "w", 0o600);
    try {
      this.child = spawn(binary, args, { cwd, env, detached: true, stdio: ["ignore", output, output] });
    } finally {
      closeSync(output);
    }
    this.exited = new Promise((resolve) => {
      const done = (exit: Exit) => { this.exit = exit; resolve(exit); };
      this.child.once("error", (error) => done({ code: null, signal: null, error }));
      this.child.once("exit", (code, signal) => done({ code, signal }));
    });
  }

  output(): string { return readFileSync(this.log, "utf8"); }

  assertAlive(): void {
    if (this.exit) throw new Error(`Service exited: ${this.log}\n${this.exit.error ?? this.exit.signal ?? this.exit.code}\n${this.output()}`);
  }

  kill(): void {
    if (this.stopped || !this.child.pid) return;
    this.stopped = true;
    try {
      // Reap the owned process group, including Vite's runtime child.
      process.kill(-this.child.pid, "SIGKILL");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
    }
  }

  async stop(): Promise<void> {
    this.kill();
    await this.exited;
  }
}

export class Processes {
  private readonly children = new Set<ManagedProcess>();
  private readonly controller = new AbortController();
  readonly signal = this.controller.signal;

  constructor(readonly logs: string) {}

  start(name: string, binary: string, args: string[], cwd: string, env: NodeJS.ProcessEnv = {}): ManagedProcess {
    this.signal.throwIfAborted();
    const child = new ManagedProcess(binary, args, cwd, {
      PATH: process.env.PATH,
      LD_LIBRARY_PATH: process.env.LD_LIBRARY_PATH,
      ...env,
    }, join(this.logs, `${name}.log`));
    this.children.add(child);
    return child;
  }

  async run(name: string, binary: string, args: string[], cwd: string, env: NodeJS.ProcessEnv = {}, timeout = 1_200_000): Promise<string> {
    const child = this.start(name, binary, args, cwd, env);
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      const exit = await Promise.race([
        child.exited,
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(new Error(`Command timed out: ${child.log}`)), timeout);
        }),
      ]);
      this.signal.throwIfAborted();
      if (exit.code !== 0 || exit.error) throw new Error(`Command failed: ${child.log}\n${exit.error ?? exit.signal ?? exit.code}\n${child.output()}`);
      return child.output();
    } finally {
      clearTimeout(timer);
      await child.stop();
      this.children.delete(child);
    }
  }

  assertAlive(): void {
    this.signal.throwIfAborted();
    for (const child of this.children) child.assertAlive();
  }

  cancel(): void {
    this.controller.abort(new Error("Storage fixture cancelled"));
    for (const child of this.children) child.kill();
  }

  async close(): Promise<void> {
    const results = await Promise.allSettled([...this.children].map((child) => child.stop()));
    this.children.clear();
    const errors = results.filter((result) => result.status === "rejected").map((result) => result.reason);
    if (errors.length) throw new AggregateError(errors, "Failed to stop fixture processes");
  }
}
