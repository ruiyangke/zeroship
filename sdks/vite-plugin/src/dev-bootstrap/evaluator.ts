/**
 * Eval-based ModuleEvaluator for the zeroship V8 runtime.
 * Same pattern as Cloudflare's __VITE_UNSAFE_EVAL__.
 */

import { ssrModuleExportsKey } from "vite/module-runner";

export const zeroshipEvaluator = {
  async runInlinedModule(
    context: Record<string, any>,
    code: string,
    _module: { id: string },
  ): Promise<void> {
    const keys = Object.keys(context).join(",");
    const wrapped = `"use strict";async (${keys})=>{${code}\n}`;
    const fn = (0, eval)(wrapped);
    await fn(...Object.values(context));
  },

  async runExternalModule(filepath: string): Promise<any> {
    // V8 runtime does not support dynamic import(). All modules should be
    // inlined by Vite (resolved via node-compat polyfills or bundled).
    // If we get here, the module was externalized — which is a config error.
    throw new Error(
      `[zeroship] Cannot import external module "${filepath}". ` +
      `The V8 runtime does not support dynamic import(). ` +
      `Add this module to the node-compat polyfills or configure Vite to bundle it.`
    );
  },
};
