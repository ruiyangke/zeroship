// CI DOC-EXAMPLE GATE (PR8) — the typed `@zeroship/migrate` examples in
// `docs/reference/migrate-op-dsl.md` must TYPECHECK against the REAL package
// types, so a future API change that rots a documented snippet FAILS CI.
//
// HOW IT WORKS. The test extracts every fenced ```ts block from the reference
// doc, assembles them into ONE synthesized TypeScript module that imports the
// real `@zeroship/migrate` surface, and runs `tsc --noEmit` over it. A snippet
// that no longer compiles (a renamed op, a dropped `t.*` factory, a changed
// signature, a removed scalar-namespace member) is a hard test failure — the doc cannot
// silently rot.
//
//   - A block that is itself a module (`export default …` / `export interface …`)
//     is emitted verbatim at module scope (the hero migration carries its own
//     `import { … } from "@zeroship/migrate"`).
//   - A bare op-call fragment (e.g. a standalone `createTable(…)` / `update(…)`)
//     is wrapped in a `function _frag_N() { … }` body, under a module preamble
//     that imports the FULL documented vocabulary, so it typechecks exactly as if
//     it sat inside an `up()`.
//
// REGRESSION WITNESS (the gate is real, not a no-op): see the
// `rotted snippet is rejected by the gate` test below — it feeds the SAME
// compiler harness a deliberately-rotted snippet (an op that does not exist) and
// asserts the harness reports a type error. If the harness ever silently passed
// bad code, that witness fails.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG_ROOT = resolve(HERE, "..");
const DOC = resolve(PKG_ROOT, "../../docs/reference/migrate-op-dsl.md");

// The full documented import vocabulary the fragment harness exposes. The
// fluent-only redesign exports just the documented core vocabulary; an op
// the doc names but the package does not export fails THIS import line at compile
// time. (The flat ops are GONE — a doc that still imports them fails the gate.)
const VOCAB_PREAMBLE = `import {
  table,
  view,
  enumType,
  comment,
  t, fromDb, lintDeterminism,
  now, genRandomUuid, currentSetting, currentUser, interval, concatWs,
} from "@zeroship/migrate";
import { pgTable } from "@zeroship/migrate/pg";
import { t as dbT } from "@zeroship/db";
`;

/** Pull every fenced ```ts block out of the markdown doc, in order.
 *
 *  A block carrying a `// ops.ts:` / `// types.ts:` source-citation comment is a
 *  SIGNATURE-listing block — documentation pseudo-code with bare parameter names
 *  (`createTable(name, columns, build?)`, `(b) => void` in value position). Those
 *  are reference signatures, not runnable code, so they are excluded from the
 *  compile harness. Everything else is a runnable example and IS compiled. */
function extractTsBlocks(md: string): string[] {
  const blocks: string[] = [];
  const re = /```ts\n([\s\S]*?)```/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(md)) !== null) {
    const block = m[1];
    if (/\/\/\s*(ops|types)\.ts:/.test(block)) continue; // skip signature listings
    blocks.push(block);
  }
  return blocks;
}

/** A module-scope block carries its own imports / is a type declaration. */
function isModuleScope(block: string): boolean {
  return /(^|\n)\s*(export\s+(default|interface|type|const|function)|import\s)/.test(block);
}

/** Assemble the doc snippets into one compilable harness module. */
function assembleHarness(blocks: string[]): string {
  const moduleBlocks: string[] = [];
  const fragmentBodies: string[] = [];
  for (const b of blocks) {
    if (isModuleScope(b)) {
      // A module example carries its own imports — but the hero's default-export
      // collides with the next module example. Wrap each module example in its
      // own namespace via a unique const alias is not possible (it's a default
      // export), so re-emit module examples inside a fresh block is not enough.
      // Instead, strip the `import … from "@zeroship/migrate"` (the harness
      // already imports the vocab) and downgrade `export default {…}` to a typed
      // local so multiple modules coexist. `export interface`/`export type`
      // become plain declarations.
      let body = b.replace(/^\s*import[^\n]*from\s+["']@zeroship\/migrate["'];?\s*$/gm, "");
      body = body.replace(/^\s*export\s+default\s+/m, `const _mod_${moduleBlocks.length}: import("@zeroship/migrate").Migration = `);
      body = body.replace(/^\s*export\s+(interface|type|const|function)\s+/m, "$1 ");
      moduleBlocks.push(body);
    } else {
      fragmentBodies.push(`function _frag_${fragmentBodies.length}() {\n${b}\n}`);
    }
  }
  return [
    "/* GENERATED doc-example harness — see tests/doc-examples.test.ts */",
    VOCAB_PREAMBLE,
    'void dbT; // referenced by some doc fragments',
    ...moduleBlocks,
    ...fragmentBodies,
  ].join("\n\n");
}

/** Run `tsc --noEmit` over a single synthesized harness file rooted in the
 *  package, so it resolves the REAL `@zeroship/migrate` + `@zeroship/db` types.
 *  Returns null on success, or the compiler diagnostics on failure. */
function typecheck(harnessSource: string): string | null {
  // The harness MUST live inside the package tree so node module resolution
  // finds the package's own `node_modules` (`@zeroship/migrate`/`@zeroship/db`).
  // A throwaway subdir UNDER `node_modules/` (already gitignored, so a crash
  // leftover is never accidentally tracked); a sibling of `@zeroship/*`, so the
  // parent-dir resolution from the harness still reaches the package's deps.
  const dir = mkdtempSync(join(PKG_ROOT, "node_modules", ".doc-gate-"));
  try {
    const file = join(dir, "doc_harness.ts");
    writeFileSync(file, harnessSource, "utf8");
    // A throwaway tsconfig that EXTENDS the package's tsconfig (same strictness,
    // same Bundler module resolution) but compiles only the harness file.
    const tsconfig = {
      extends: resolve(PKG_ROOT, "tsconfig.json"),
      compilerOptions: { noEmit: true, rootDir: dir, types: [] as string[] },
      include: ["doc_harness.ts"],
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

test("doc-gate: every typed example in migrate-op-dsl.md typechecks against the real package", () => {
  const md = readFileSync(DOC, "utf8");
  const blocks = extractTsBlocks(md);
  assert.ok(blocks.length >= 6, `expected the doc to carry several runnable ts examples; found ${blocks.length}`);
  const harness = assembleHarness(blocks);
  const diagnostics = typecheck(harness);
  assert.equal(
    diagnostics,
    null,
    `a typed example in docs/reference/migrate-op-dsl.md no longer compiles against @zeroship/migrate.\n` +
      `Fix the doc (or the snippet) — do not weaken this gate.\n\n${diagnostics ?? ""}`,
  );
});

test("doc-gate REGRESSION WITNESS: a rotted snippet IS rejected by the gate", () => {
  // A snippet naming an op that does not exist (the exact failure mode a future
  // API rename would create in the doc). The harness MUST report a type error;
  // if it passes, the gate is a no-op and this test fails RED.
  const rotted = assembleHarness([`thisOpWasRenamedAndNoLongerExists("users", t.text());`]);
  const diagnostics = typecheck(rotted);
  assert.notEqual(
    diagnostics,
    null,
    "the doc-example gate accepted a snippet calling a non-existent op — the gate is not actually typechecking",
  );
  assert.match(
    diagnostics ?? "",
    /thisOpWasRenamedAndNoLongerExists|Cannot find name|TS2304/,
    `expected a 'cannot find name' diagnostic for the rotted op; got:\n${diagnostics}`,
  );
});
