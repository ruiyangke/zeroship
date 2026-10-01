import { promises as fs } from "node:fs";
import { join, relative } from "node:path";
import {
  createBuilder,
  loadConfigFromFile,
  mergeConfig,
  type InlineConfig,
  type LogLevel,
  type Plugin,
  type PluginOption,
  type ViteBuilder,
} from "vite";
import { BUILD_PLUGIN_NAME, type ZeroshipBuildApi } from "./build.js";
import { computeManifestExtras } from "./manifest.js";
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

/** The dev server's own config, which every dev archive is built from. */
export interface DevBuildConfig {
  /** The dev server's root. */
  root: string;
  /** The config file the dev server loaded; undefined when it loaded none. */
  configFile: string | undefined;
  /** The dev server's inline config. */
  inlineConfig: InlineConfig;
  /** The dev server's mode. */
  mode: string;
  /** Fresh zeroship plugins, from the options the dev server's own were built with. */
  plugins: () => PluginOption[];
}

/** The local app deployment, ready for the host to ingest. */
export interface DevBundle {
  archive: Buffer;
  /** Sources observed by the build, used to invalidate local snapshots. */
  dependencies: string[];
}

async function flattenPlugins(options: PluginOption[] | undefined): Promise<Plugin[]> {
  const plugins: Plugin[] = [];
  for (const option of options ?? []) {
    const value = await option;
    if (!value) continue;
    if (Array.isArray(value)) plugins.push(...await flattenPlugins(value));
    else plugins.push(value as Plugin);
  }
  return plugins;
}

/**
 * A builder for one dev archive, from the dev server's own config.
 *
 * The config file is loaded again, so the app's plugins are new instances
 * that share no state with the dev server's or with an earlier archive's.
 * Zeroship's own plugins, which the config file and the inline config both
 * carry, are replaced by fresh ones from the dev server's options. Other
 * plugins in the inline config are the dev server's own instances.
 */
async function createArchiveBuilder(config: DevBuildConfig): Promise<ViteBuilder> {
  const own = await flattenPlugins(config.plugins());
  const ownNames = new Set(own.map((plugin) => plugin.name));
  const appPlugins = (plugins: Plugin[]) => plugins.filter((plugin) => !ownNames.has(plugin.name));
  const { plugins: inlinePlugins, configFile: _configFile, ...inline } = config.inlineConfig;
  const loaded = config.configFile === undefined
    ? null
    : await loadConfigFromFile(
        { command: "build", mode: config.mode, isSsrBuild: false, isPreview: false },
        config.configFile,
        config.root,
        inline.logLevel,
        inline.customLogger,
        inline.configLoader,
      );
  const { plugins: filePlugins, ...file } = loaded?.config ?? {};
  const merged: InlineConfig = mergeConfig(file, inline);
  // A rebuild on every edit: warnings, not Vite's build report.
  const quiet: LogLevel[] = ["error", "silent"];
  return createBuilder({
    ...merged,
    root: config.root,
    mode: config.mode,
    configFile: false,
    logLevel: merged.logLevel !== undefined && quiet.includes(merged.logLevel) ? merged.logLevel : "warn",
    plugins: [
      ...appPlugins(await flattenPlugins(filePlugins)),
      ...appPlugins(await flattenPlugins(inlinePlugins)),
      ...own,
    ],
  }, null);
}

/**
 * Build the dev archive: the app's worker and declarations, compiled the way
 * `vite build` compiles them, from the dev server's config, with `NODE_ENV`
 * the dev tier's `development`. Vite serves client assets during
 * development. The caller owns publication ordering when sources change
 * during a build. Host credentials stay outside it. A failed worker build
 * throws a `WorkerBuildError` naming the sources it read.
 */
export async function buildDevBundle(opts: {
  config: DevBuildConfig;
  project: ResolvedProjectConfig;
  databases: DevDatabase[];
}): Promise<DevBundle> {
  const root = opts.config.root;
  const stateDir = join(root, ".zeroship");
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
        out: relative(root, out),
      });
    }
    // A fresh builder, and with it fresh plugins and transform state, per
    // archive: deleted exports and schedules must not survive a rebuild.
    const builder = await createArchiveBuilder(opts.config);
    const api = builder.config.plugins.find((plugin) => plugin.name === BUILD_PLUGIN_NAME)?.api as
      | ZeroshipBuildApi
      | undefined;
    if (api == null) throw new Error(`[zeroship] the dev archive's builder has no ${BUILD_PLUGIN_NAME} plugin`);
    const { state, dependencies } = await api.buildDevWorker(builder, {
      outDir: join(dist, "server"),
      nodeEnv: "development",
    });
    const extras = await computeManifestExtras({
      root,
      procedures: state.discoveredProcedures,
      schedules: state.discoveredSchedules,
      workflowNames: state.discoveredWorkflows.map(workflow => workflow.exportName),
      mode: "development",
    });
    const bundle = await emitZship({
      root,
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
