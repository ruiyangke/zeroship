// The runtime owns the zeroship ESM namespace in built apps and ModuleRunner dev.
import type { Plugin } from "vite";
import { RUNTIME_MODULE_SPECIFIER } from "./constants.js";

/**
 * Prefix of the modules that stand in for a CommonJS `require` of a runtime
 * module. The isolate has no `require`, and Rolldown turns a require left
 * pointing at an external into a shim that throws, so the require is pointed
 * at one of these instead: an ES module that re-exports the runtime module,
 * which Rolldown bundles and hands the require as its CommonJS view.
 */
export const RUNTIME_REQUIRE_PREFIX = "\0zeroship-require:";

/** The ES module standing in for a CommonJS `require` of `specifier`. */
export function runtimeRequireModule(specifier: string): string {
  const source = JSON.stringify(specifier);
  // The kernel module has only named exports.
  return specifier === RUNTIME_MODULE_SPECIFIER
    ? `export * from ${source};\n`
    : `export * from ${source};\nexport { default } from ${source};\n`;
}

export function zeroshipModulePlugin(): Plugin {
  return {
    name: "zeroship:runtime-module",
    enforce: "pre",
    resolveId(id: string, _importer: string | undefined, options?: { kind?: string }) {
      if (id !== RUNTIME_MODULE_SPECIFIER) return null;
      return options?.kind === "require-call"
        ? RUNTIME_REQUIRE_PREFIX + id
        : { id, external: true };
    },
    load(id: string) {
      return id.startsWith(RUNTIME_REQUIRE_PREFIX)
        ? runtimeRequireModule(id.slice(RUNTIME_REQUIRE_PREFIX.length))
        : null;
    },
  };
}
