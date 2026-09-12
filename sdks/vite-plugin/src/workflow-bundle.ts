import { promises as fs } from "node:fs";
import { join, relative } from "node:path";
import { buildServerBundle } from "./build.js";
import { computeManifestExtras } from "./manifest.js";
import type { TransformState } from "./transform.js";
import type { ResolvedProjectConfig } from "./project-config/index.js";
import { RUNTIME_DESCRIPTOR_FILE } from "./gen-types/index.js";
import { emitZship } from "./zship.js";

/** A built local executable, ready for the customer host to retain. */
export interface WorkflowBundle {
  archive: Buffer;
  /** Sources observed by the build, used to invalidate local snapshots. */
  dependencies: string[];
}

/**
 * Freeze the app's workflow graph using the deployment compiler and archive
 * format. The caller owns publication ordering when sources change during a
 * build. Runtime environment variables and host credentials stay outside it.
 */
export async function buildWorkflowBundle(opts: {
  root: string;
  entry: string;
  project: ResolvedProjectConfig;
  runtimeDescriptor: string | undefined;
}): Promise<WorkflowBundle> {
  const stateDir = join(opts.root, ".zeroship");
  await fs.mkdir(stateDir, { recursive: true });
  const staging = await fs.mkdtemp(join(stateDir, "workflow-build-"));
  try {
    const dist = join(staging, "dist");
    const generated = join(staging, "generated");
    if (opts.runtimeDescriptor !== undefined) {
      await fs.mkdir(generated, { recursive: true });
      await fs.writeFile(join(generated, RUNTIME_DESCRIPTOR_FILE), opts.runtimeDescriptor);
    }
    // Each build discovers its own declarations; deleted exports and schedules
    // must not survive through the dev environment's accumulated transform state.
    const state: TransformState = {
      serverFunctionMap: new Map(),
      discoveredProcedures: [],
      discoveredSchedules: [],
      discoveredWorkflows: [],
    };
    const dependencies = await buildServerBundle({
      root: opts.root,
      entry: opts.entry,
      outDir: join(dist, "server"),
      clientDistDir: opts.project.build.dist,
      state,
    });
    const extras = await computeManifestExtras({
      root: opts.root,
      procedures: state.discoveredProcedures,
      schedules: state.discoveredSchedules,
      workflowNames: state.discoveredWorkflows.map(workflow => workflow.exportName),
      mode: "development",
    });
    const bundle = await emitZship({
      root: opts.root,
      runtimeDate: opts.project.runtime_date,
      distDir: dist,
      outputPath: join(staging, "workflows.zship"),
      silent: true,
      precompress: { brotli: false, gzip: false },
      rpcExtras: extras,
      migrations: {
        dir: opts.project.migrations.dir,
        genTypesOut: relative(opts.root, generated),
      },
    });
    return { archive: await fs.readFile(bundle.outputPath), dependencies };
  } finally {
    await fs.rm(staging, { recursive: true, force: true });
  }
}
