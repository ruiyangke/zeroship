// The runtime owns the zeroship ESM namespace in built apps and ModuleRunner dev.
import type { Plugin } from "vite";

export function zeroshipModulePlugin(): Plugin {
  return {
    name: "zeroship:runtime-module",
    enforce: "pre",
    resolveId(id: string) {
      return id === "zeroship" ? { id, external: true } : null;
    },
  };
}
