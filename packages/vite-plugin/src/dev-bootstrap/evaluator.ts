/**
 * Eval-based ModuleEvaluator for the zeroship V8 runtime.
 * Same pattern as Cloudflare's __VITE_UNSAFE_EVAL__.
 */

import {
  RUNTIME_MODULE_SPECIFIER,
  VITE_RUNTIME_MODULE_ID,
} from "../constants.js";

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
    // The host resolves this reserved module and shares its native exports
    // with the bundled entry. Creator dependencies still belong to Vite.
    if (filepath === RUNTIME_MODULE_SPECIFIER || filepath === VITE_RUNTIME_MODULE_ID) {
      return import(RUNTIME_MODULE_SPECIFIER);
    }
    throw new Error(
      `[zeroship] Cannot import external module "${filepath}". ` +
      `Configure Vite to bundle this dependency or provide a runtime module adapter.`
    );
  },
};
