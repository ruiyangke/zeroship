// The back office's own Vite config, plus one plugin that records which
// modules the worker build bundled, for `tests/worker-build.test.ts`.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import type { Plugin, UserConfig } from "vite";
import config from "../../vite.config";

export const workerModulesPath = fileURLToPath(
  new URL("../.artifacts/worker-modules.json", import.meta.url),
);

const recordWorkerModules: Plugin = {
  name: "test:worker-modules",
  apply: "build",
  applyToEnvironment: (environment) => environment.name === "zeroship",
  generateBundle(_options, bundle) {
    const ids = new Set<string>();
    for (const output of Object.values(bundle))
      if (output.type === "chunk") for (const id of output.moduleIds) ids.add(id);
    mkdirSync(dirname(workerModulesPath), { recursive: true });
    writeFileSync(workerModulesPath, JSON.stringify([...ids].sort()));
  },
};

export default {
  ...(config as UserConfig),
  plugins: [...((config as UserConfig).plugins ?? []), recordWorkerModules],
} satisfies UserConfig;
