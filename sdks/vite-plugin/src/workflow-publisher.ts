import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import type { WorkflowBundle } from "./workflow-bundle.js";

/** Serialize rebuilds and replace the host's archive only with a complete image. */
export class WorkflowPublisher {
  private revision = 0;
  private running: Promise<void> | undefined;
  private closed = false;

  constructor(
    readonly path: string,
    private readonly build: () => Promise<WorkflowBundle>,
    private readonly observe: (dependencies: string[]) => void,
  ) {}

  refresh(): Promise<void> {
    if (this.closed) return Promise.reject(new Error("workflow publisher is closed"));
    this.revision += 1;
    this.running ??= this.drain().finally(() => { this.running = undefined; });
    return this.running;
  }

  close(): Promise<void> {
    this.closed = true;
    return this.running?.catch(() => {}) ?? Promise.resolve();
  }

  private async drain(): Promise<void> {
    while (!this.closed) {
      const revision = this.revision;
      let bundle: WorkflowBundle;
      try {
        bundle = await this.build();
      } catch (error) {
        if (this.closed) return;
        if (revision !== this.revision) continue;
        throw error;
      }
      if (this.closed) return;
      this.observe(bundle.dependencies);
      if (revision !== this.revision) continue;
      const directory = dirname(this.path);
      await fs.mkdir(directory, { recursive: true });
      const staging = await fs.mkdtemp(join(directory, "workflow-publish-"));
      try {
        const pending = join(staging, "bundle.zship");
        await fs.writeFile(pending, bundle.archive);
        if (this.closed) return;
        if (revision !== this.revision) continue;
        await fs.rename(pending, this.path);
      } finally {
        await fs.rm(staging, { recursive: true, force: true });
      }
      if (revision === this.revision) return;
    }
  }
}
