// The runtime owns the zeroship ESM namespace in built apps and ModuleRunner dev.
import type { Plugin } from "vite";

/** Resolve framework dependencies through the plugin's installation. */
function resolveBootstrapPkgRoot(): string | undefined {
  try {
    const mainUrl = new URL(import.meta.resolve("@zeroship/bootstrap"));
    return mainUrl.pathname.replace(/[/\\]dist[/\\][^/\\]+$/, "");
  } catch {
    return undefined;
  }
}

export function zeroshipBootstrapResolverPlugin(): Plugin {
  const pkgRoot = resolveBootstrapPkgRoot();
  const dbInternal = new URL(import.meta.resolve("@zeroship/db/internal")).pathname;
  return {
    name: "zeroship:bootstrap-resolver",
    enforce: "pre" as const,
    resolveId(id: string) {
      if (id === "@zeroship/db/internal") return dbInternal;
      if (!pkgRoot) return null;
      if (id === "@zeroship/bootstrap") {
        return `${pkgRoot}/dist/index.js`;
      }
      if (id.startsWith("@zeroship/bootstrap/")) {
        const sub = id.slice("@zeroship/bootstrap/".length);
        return `${pkgRoot}/dist/${sub}.js`;
      }
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
