/**
 * Packages the plugin loads at run time must be declared.
 *
 * The plugin reaches for a few packages through a `dynamicImport(...)` shim
 * rather than a static import, so the bundler never sees them and nothing
 * fails at build time. Inside this monorepo they resolve anyway - every
 * workspace package is hoisted into the root `node_modules` - so an
 * undeclared dependency is invisible here and only surfaces for someone who
 * installed `@zeroship/vite-plugin` from the registry, where the load throws
 * and the feature it backs silently does not compile.
 *
 * Scanning for the shim's own call shape keeps this honest without guessing:
 * a bare specifier passed to `dynamicImport` is always a package the plugin
 * itself loads, never one it merely emits into generated app code.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const pkgRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...sourceFiles(full));
    else if (entry.name.endsWith(".ts")) out.push(full);
  }
  return out;
}

/** `@scope/name/sub` -> `@scope/name`; `name/sub` -> `name`. */
function packageOf(specifier: string): string {
  const parts = specifier.split("/");
  return specifier.startsWith("@") ? parts.slice(0, 2).join("/") : parts[0];
}

describe("runtime dependencies are declared", () => {
  test("every dynamically imported package appears in package.json", () => {
    const pkg = JSON.parse(readFileSync(join(pkgRoot, "package.json"), "utf8")) as {
      dependencies?: Record<string, string>;
      peerDependencies?: Record<string, string>;
      optionalDependencies?: Record<string, string>;
    };
    const declared = new Set([
      ...Object.keys(pkg.dependencies ?? {}),
      ...Object.keys(pkg.peerDependencies ?? {}),
      ...Object.keys(pkg.optionalDependencies ?? {}),
    ]);

    const found = new Map<string, string>();
    for (const file of sourceFiles(join(pkgRoot, "src"))) {
      const code = readFileSync(file, "utf8");
      // Bare specifiers only. A relative path or a `new URL(...)` argument is
      // an in-tree fallback, not a package the consumer has to install.
      for (const m of code.matchAll(/dynamicImport\(\s*["']([^."'][^"']*)["']\s*\)/g)) {
        found.set(packageOf(m[1]), file.slice(pkgRoot.length + 1));
      }
    }

    assert.ok(found.size > 0, "the scan must find the dynamicImport call sites");

    const undeclared = [...found].filter(([name]) => !declared.has(name));
    assert.deepEqual(
      undeclared,
      [],
      `dynamically imported but not declared in package.json: ${undeclared
        .map(([name, file]) => `${name} (${file})`)
        .join(", ")}`,
    );
  });
});
