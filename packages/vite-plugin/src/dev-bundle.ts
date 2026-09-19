import { promises as fs } from "node:fs";
import { join, relative } from "node:path";
import { buildServerBundle } from "./build.js";
import { computeManifestExtras } from "./manifest.js";
import type { TransformState } from "./transform.js";
import type { ResolvedProjectConfig } from "./project-config/index.js";
import { RUNTIME_DESCRIPTOR_FILE } from "./gen-types/index.js";
import { emitZship } from "./zship.js";

/** One database the dev deployment declares, with its already-folded schema. */
export interface DevDatabase {
  /** The creator's LOCAL label, which reaches the runtime through the manifest. */
  label: string;
  /** The typed id declared in `zeroship.jsonc` and dereferenced before packing. */
  id: string;
  /** Whether this is the app's `env.db`. */
  primary: boolean;
  /** This database's migration sources, for the packer's missing-fold refusal. */
  migrations: string;
  /** The folded `schema.runtime.json` text for this database. */
  descriptor: string;
}

/** The local app deployment, ready for the host to ingest. */
export interface DevBundle {
  archive: Buffer;
  /** Sources observed by the build, used to invalidate local snapshots. */
  dependencies: string[];
}

/**
 * Build the app's server graph and declarations with the deployment compiler.
 * Vite serves client assets during development. The caller owns publication
 * ordering when sources change during a build. Host credentials stay outside it.
 */
export async function buildDevBundle(opts: {
  root: string;
  entry: string;
  project: ResolvedProjectConfig;
  databases: DevDatabase[];
}): Promise<DevBundle> {
  const stateDir = join(opts.root, ".zeroship");
  await fs.mkdir(stateDir, { recursive: true });
  const staging = await fs.mkdtemp(join(stateDir, "app-build-"));
  try {
    const dist = join(staging, "dist");
    const generated = join(staging, "generated");
    // One staged descriptor per database, each under its own label, because
    // the three gen-types filenames are fixed and a shared directory would be
    // one database's schema standing in for another's.
    const packed = [];
    for (const database of opts.databases) {
      const out = join(generated, database.label);
      await fs.mkdir(out, { recursive: true });
      await fs.writeFile(join(out, RUNTIME_DESCRIPTOR_FILE), database.descriptor);
      packed.push({
        label: database.label,
        id: database.id,
        primary: database.primary,
        migrations: database.migrations,
        out: relative(opts.root, out),
      });
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
      outputPath: join(staging, "app.zship"),
      silent: true,
      precompress: { brotli: false, gzip: false },
      rpcExtras: extras,
      databases: packed,
    });
    return { archive: await fs.readFile(bundle.outputPath), dependencies };
  } finally {
    await fs.rm(staging, { recursive: true, force: true });
  }
}
