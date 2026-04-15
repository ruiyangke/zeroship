/**
 * Eval-based ModuleEvaluator for the zeroship V8 runtime.
 * Same pattern as Cloudflare's __VITE_UNSAFE_EVAL__.
 */

const SSR_MODULE_EXPORTS_KEY = "__vite_ssr_exports__";

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
    Object.seal(context[SSR_MODULE_EXPORTS_KEY]);
  },

  async runExternalModule(filepath: string): Promise<any> {
    return import(filepath);
  },
};
