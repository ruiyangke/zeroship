// CI DOC-EXAMPLE GATE (host side) — every typed `zero-migrate-cli` (host API)
// example in `docs/getting-started.md` AND `docs/node-api.md` must TYPECHECK
// against the REAL engine package types, so an API change that rots a documented
// snippet FAILS CI.
//
// The DSL (`zero-migrate`) examples in those docs are gated by the DSL package's
// `tests/doc-examples.test.ts`; this gate covers ONLY blocks importing
// `zero-migrate-cli` (the host/facade surface). This package can resolve
// `zero-migrate-cli`'s own source, so those snippets are compiled here.
//
// `node-api.md` was added after a sweep found it gated by NOTHING: the DSL gate
// reads `writing-migrations.md` and `getting-started.md`, this one read only
// `getting-started.md`, and the document describing the API both gates protect was
// the one neither of them opened.
//
// HOW IT WORKS. Extract every fenced ```ts / ```typescript block, keep the ones
// importing `zero-migrate-cli`, rewrite the imports so tsc resolves the real
// engine source (`../src/index.ts`) and stub any relative migration import (the
// doc's `import … from "./migrations/…"` has no real file), then run `tsc --noEmit`
// over each block SEPARATELY - snippets are standalone and legitimately reuse
// names like `apply` and `policy`.
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
const NODE_API_DOC = resolve(PKG_ROOT, "../../docs/node-api.md");

/** Pull every fenced ```ts block that imports `zero-migrate-cli`. */
function extractEngineBlocks(md: string): string[] {
  const blocks: string[] = [];
  // Accept BOTH fence spellings. Matching only ```ts made every ```typescript
  // block invisible to this gate - three of them across the gated docs - so the
  // gate silently covered less than it claimed.
  const re = /```(?:ts|typescript)\n([\s\S]*?)```/g;
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
  // Any `import * as <name> from "./relative/path"` becomes a typed stub under the
  // SAME name, so the snippet still exercises the real option types. The binding
  // name is captured rather than hardcoded: the reference doc names its imports
  // after the migration (`renameUsersDisplayName`), not `migration`, and a rule
  // that only matched `migration` left those blocks referring to a module that
  // does not exist. Only RELATIVE specifiers are rewritten - a package import is
  // real and must keep resolving.
  body = body.replace(
    /^\s*import\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s+["']\.[^"']*["'];?\s*$/gm,
    (_match, binding: string) =>
      `const ${binding}: import(${JSON.stringify(srcIndex)}).MigrationModule = { up() {} };`,
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

/** Typecheck every block SEPARATELY and return the first failure, or null.
 *
 *  Blocks are compiled one at a time rather than concatenated into one module.
 *  A documentation snippet is meant to stand on its own, and several of them
 *  legitimately declare the same names - `apply`, `plan`, `migration`, `policy`,
 *  `report` all recur across the reference. Concatenating produced a wall of
 *  TS2300/TS2451 duplicate-identifier errors that say nothing about the docs.
 *  The original gate never hit this because it only ever extracted one block. */
function typecheckEach(blocks: string[], doc: string): string | null {
  for (const [index, block] of blocks.entries()) {
    const diagnostics = typecheck(rewriteForGate(block, SRC_INDEX));
    if (diagnostics !== null) {
      return `${doc} example #${index + 1} does not compile:\n${block}\n\n${diagnostics}`;
    }
  }
  return null;
}

test("doc-gate: the zero-migrate-cli embedding example in getting-started.md typechecks", () => {
  const md = readFileSync(GETTING_STARTED_DOC, "utf8");
  const blocks = extractEngineBlocks(md);
  assert.ok(
    blocks.length >= 3,
    `expected getting-started.md to carry its zero-migrate-cli examples; found ` +
      `${blocks.length}. It has three once BOTH fence spellings are read - a drop ` +
      `to one means the extractor regressed to the ts-only fence.`,
  );
  const diagnostics = typecheckEach(blocks, "docs/getting-started.md");
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

test("doc-gate: every zero-migrate-cli example in node-api.md typechecks", () => {
  // `docs/node-api.md` is the reference for this package's public surface, and
  // until this arm existed it was gated by nothing. The DSL gate covers
  // `writing-migrations.md` and `getting-started.md`; the arm above covers the one
  // embedding block in `getting-started.md`. The document describing the API those
  // gates protect was the document neither of them read.
  //
  // It carries far more examples than the guide does, so the threshold is set
  // against what is there now: a future edit that deletes the examples rather than
  // fixing them fails here instead of quietly emptying the gate.
  const md = readFileSync(NODE_API_DOC, "utf8");
  const blocks = extractEngineBlocks(md);
  assert.ok(
    blocks.length >= 3,
    `expected node-api.md to carry its zero-migrate-cli examples; found ${blocks.length}`,
  );
  const diagnostics = typecheckEach(blocks, "docs/node-api.md");
  assert.equal(
    diagnostics,
    null,
    `an example in docs/node-api.md no longer compiles against zero-migrate-cli.\n` +
      `Fix the doc (or the snippet) — do not weaken this gate.\n\n${diagnostics ?? ""}`,
  );
});
