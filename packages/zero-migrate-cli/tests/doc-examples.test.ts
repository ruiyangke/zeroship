// CI DOC-EXAMPLE GATE (host side) — the typed `zero-migrate-cli` (host API)
// example in `docs/getting-started.md` must TYPECHECK against the REAL engine
// package types, so a future API change that rots the documented embedding
// snippet FAILS CI.
//
// The DSL (`zero-migrate`) examples in that same doc are gated by the DSL
// package's `tests/doc-examples.test.ts`; this gate covers ONLY the block that
// imports `zero-migrate-cli` (the host/facade surface). This package can
// resolve `zero-migrate-cli`'s own source, so the snippet is compiled here.
//
// HOW IT WORKS. Extract every fenced ```ts block from the guide, keep the ones
// importing `zero-migrate-cli`, rewrite the imports so tsc resolves the real
// engine source (`../src/index.ts`) and a typed migration stub (the doc's
// `import … from "./migrations/…"` has no real file), and run `tsc --noEmit`.
//
// REGRESSION WITNESS: a deliberately-rotted engine snippet (a verb that does not
// exist) MUST be rejected — see the second test.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG_ROOT = resolve(HERE, "..");
const GETTING_STARTED_DOC = resolve(PKG_ROOT, "../../docs/getting-started.md");

/** Pull every fenced ```ts block that imports `zero-migrate-cli`. */
function extractEngineBlocks(md: string): string[] {
  const blocks: string[] = [];
  const re = /```ts\n([\s\S]*?)```/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(md)) !== null) {
    if (/from\s+["']zero-migrate-cli["']/.test(m[1])) blocks.push(m[1]);
  }
  return blocks;
}

/** Rewrite a doc snippet so tsc can resolve it against the real engine source.
 *  - `from "zero-migrate-cli"` → the package's own `src/index.ts`.
 *  - the doc's `import * as migration from "./migrations/…"` (no real file) →
 *    a typed `MigrationModule` stub, so the snippet still exercises the REAL
 *    `apply`/`plan` option types. */
function rewriteForGate(block: string, srcIndex: string): string {
  let body = block.replace(
    /from\s+["']zero-migrate-cli["']/g,
    `from ${JSON.stringify(srcIndex)}`,
  );
  body = body.replace(
    /^\s*import\s+\*\s+as\s+migration\s+from\s+["'][^"']*["'];?\s*$/m,
    `const migration: import(${JSON.stringify(srcIndex)}).MigrationModule = { up() {} };`,
  );
  return body;
}

/** Run `tsc --noEmit` over a synthesized harness rooted in the engine package,
 *  so it resolves the REAL engine + `zero-migrate` types. Returns null on
 *  success, or the compiler diagnostics on failure. */
function typecheck(harnessSource: string): string | null {
  const dir = mkdtempSync(join(PKG_ROOT, "node_modules", ".doc-gate-"));
  try {
    const file = join(dir, "doc_harness.ts");
    writeFileSync(file, harnessSource, "utf8");
    const tsconfig = {
      extends: resolve(PKG_ROOT, "tsconfig.json"),
      compilerOptions: { noEmit: true, rootDir: null, types: [] as string[] },
      include: [file],
    };
    const cfg = join(dir, "tsconfig.json");
    writeFileSync(cfg, JSON.stringify(tsconfig), "utf8");
    try {
      execFileSync(
        resolve(PKG_ROOT, "node_modules/.bin/tsc"),
        ["--noEmit", "-p", cfg],
        { cwd: PKG_ROOT, stdio: "pipe" },
      );
      return null;
    } catch (e) {
      const err = e as { stdout?: Buffer; stderr?: Buffer };
      return `${err.stdout?.toString() ?? ""}${err.stderr?.toString() ?? ""}`;
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// Extensionless module path — Bundler resolution maps it to `src/index.ts`
// (importing a literal `.ts` path needs `allowImportingTsExtensions`).
const SRC_INDEX = resolve(PKG_ROOT, "src/index");

test("doc-gate: the zero-migrate-cli embedding example in getting-started.md typechecks", () => {
  const md = readFileSync(GETTING_STARTED_DOC, "utf8");
  const blocks = extractEngineBlocks(md);
  assert.ok(
    blocks.length >= 1,
    `expected getting-started.md to carry a zero-migrate-cli embedding example; found ${blocks.length}`,
  );
  const harness = blocks.map((b) => rewriteForGate(b, SRC_INDEX)).join("\n\n");
  const diagnostics = typecheck(harness);
  assert.equal(
    diagnostics,
    null,
    `the zero-migrate-cli embedding example in docs/getting-started.md no longer ` +
      `compiles against zero-migrate-cli.\nFix the doc (or the snippet) — do not ` +
      `weaken this gate.\n\n${diagnostics ?? ""}`,
  );
});

test("doc-gate REGRESSION WITNESS: a rotted engine snippet IS rejected", () => {
  const rotted = rewriteForGate(
    `import { thisEngineVerbWasRenamed } from "zero-migrate-cli";\n` +
      `void thisEngineVerbWasRenamed;`,
    SRC_INDEX,
  );
  const diagnostics = typecheck(rotted);
  assert.notEqual(
    diagnostics,
    null,
    "the engine doc-gate accepted an import of a non-existent verb — the gate is not typechecking",
  );
  assert.match(
    diagnostics ?? "",
    /thisEngineVerbWasRenamed|has no exported member|TS2305|TS2724/,
    `expected a 'no exported member' diagnostic for the rotted verb; got:\n${diagnostics}`,
  );
});
