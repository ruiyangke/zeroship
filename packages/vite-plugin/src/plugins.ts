// packages/vite-plugin/src/plugins.ts
//
// The plugin chain `zeroship()` returns, built from an explicit process
// environment. `zeroship()` passes `process.env`; this takes the environment
// as an argument, so a caller that means a different one - a test - states it
// instead of writing to its own. Not part of the package's exports.

import type { Plugin } from "vite";
import type { ZeroshipOptions } from "./index.js";
import { transformPlugin, type TransformState } from "./transform.js";
import {
  createProjectConfigHolder,
  type ProjectConfigHolder,
  type ProjectConfigInput,
} from "./project-config/index.js";
import { devServerPlugin } from "./dev-server.js";
import { buildPlugin } from "./build.js";
import { nodeCompatPlugin, nodeInjectPlugin } from "./node-compat.js";
import { zeroshipModulePlugin } from "./zeroship-module.js";

export function zeroshipPlugins(
  options: ZeroshipOptions,
  processEnv: NodeJS.ProcessEnv,
): Plugin[] {
  // Shared state across plugins
  const state: TransformState = {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
    discoveredSchedules: [],
    discoveredWorkflows: [],
  };

  // ONE reader, shared by the build and dev-server plugins. Two independent
  // reads of the same file is how the two halves of one tool come to disagree,
  // which is the shape this whole change exists to remove -- so the holder
  // memoises per root and both plugins take the same instance. The dev server
  // reads the SAME environment the locator does.
  const input: ProjectConfigInput = {
    configPath: options.configPath,
    environment: options.env,
    override: options.config,
    processEnv,
  };
  const project: ProjectConfigHolder = createProjectConfigHolder(input);

  return [
    nodeCompatPlugin(),
    nodeInjectPlugin(),
    zeroshipModulePlugin(),
    transformPlugin(state),
    ...devServerPlugin({ ...options, processEnv }, state, project),
    buildPlugin(state, project, options.app),
  ];
}
