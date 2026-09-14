// The runtime owns the zeroship ESM namespace in built apps and ModuleRunner dev.
import type { Plugin } from "vite";

/** Resolve framework-private SDK modules through the plugin's installation. */
export function zeroshipFrameworkResolverPlugin(): Plugin {
  const dbInternal = new URL(import.meta.resolve("@zeroship/db/internal")).pathname;
  return {
    name: "zeroship:framework-resolver",
    enforce: "pre" as const,
    resolveId(id: string) {
      if (id === "@zeroship/db/internal") return dbInternal;
      return null;
    },
  };
}

export function zeroshipModulePlugin(): Plugin {
  return {
    name: "zeroship:runtime-module",
    enforce: "pre",
    resolveId(id: string) {
      return id === "zeroship" ? { id, external: true } : null;
    },
  };
}
