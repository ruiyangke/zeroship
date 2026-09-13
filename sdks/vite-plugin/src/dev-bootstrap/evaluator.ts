/**
 * Eval-based ModuleEvaluator for the zeroship V8 runtime.
 * Same pattern as Cloudflare's __VITE_UNSAFE_EVAL__.
 */

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
    if (filepath === "zeroship") return import("zeroship");
    throw new Error(
      `[zeroship] Cannot import external module "${filepath}". ` +
      `Configure Vite to bundle this dependency or provide a runtime module adapter.`
    );
  },
};
