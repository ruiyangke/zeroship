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
// The doc list grew twice. `node-api.md` was added after a sweep found it gated by
// NOTHING - the DSL gate read `writing-migrations.md` and `getting-started.md`, this
// one read only `getting-started.md`, and the document describing the API both gates
// protect was the one neither opened. `architecture.md`, `operations.md` and
// `troubleshooting.md` followed from surveying every fenced typed block in `docs/`
// rather than waiting for the next one to surface.
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
      `const ${binding}: import(${JSON.stringify(srcIndex)}).MigrationModule = { schema() {} };`,
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

/** How a document's examples are written, which decides how they must compile.
 *
 *  `standalone` - each block is a complete example a reader could paste on its own,
 *  as in the API reference. Compiled one at a time: several legitimately reuse the
 *  names `apply`, `plan`, `migration`, `policy` and `report`, so concatenating them
 *  produces TS2300/TS2451 duplicate-identifier noise that says nothing about the docs.
 *
 *  `sequence` - the blocks continue one another, as in the operations guide, where a
 *  later snippet uses the `pending` contract an earlier one obtained. Compiled
 *  together, because reading them apart would report an undefined name that the
 *  document has in fact defined.
 *
 *  Picking the wrong mode makes the gate lie in one of two directions, which is why
 *  it is declared per document rather than inferred. */
type DocShape = "standalone" | "sequence";

function typecheckDoc(blocks: string[], doc: string, shape: DocShape): string | null {
  if (shape === "sequence") {
    const harness = blocks.map((b) => rewriteForGate(b, SRC_INDEX)).join("\n\n");
    const diagnostics = typecheck(harness);
    return diagnostics === null ? null : `${doc} (read as one sequence)\n\n${diagnostics}`;
  }
  for (const [index, block] of blocks.entries()) {
    const diagnostics = typecheck(rewriteForGate(block, SRC_INDEX));
    if (diagnostics !== null) {
      return `${doc} example #${index + 1} does not compile:\n${block}\n\n${diagnostics}`;
    }
  }
  return null;
}

/** Every doc carrying `zero-migrate-cli` examples, with the number it must still
 *  carry.
 *
 *  The count is not decoration. It is what stops a future edit from deleting the
 *  examples, or the extractor from silently narrowing, and leaving a gate that
 *  passes because it found nothing to check. Each number is measured against the
 *  doc as it stands, not guessed.
 *
 *  `architecture.md`, `operations.md` and `troubleshooting.md` joined after a survey
 *  found gateable host examples across the docs that no gate read. */
const HOST_DOCS: ReadonlyArray<
  readonly [file: string, minBlocks: number, shape: DocShape]
> = [
  ["getting-started.md", 3, "standalone"],
  ["node-api.md", 3, "standalone"],
  ["architecture.md", 1, "standalone"],
  ["operations.md", 3, "sequence"],
  ["troubleshooting.md", 1, "standalone"],
];

for (const [file, minBlocks, shape] of HOST_DOCS) {
  test(`doc-gate: every zero-migrate-cli example in ${file} typechecks`, () => {
    const md = readFileSync(resolve(PKG_ROOT, "../../docs", file), "utf8");
    const blocks = extractEngineBlocks(md);
    assert.ok(
      blocks.length >= minBlocks,
      `expected docs/${file} to carry at least ${minBlocks} zero-migrate-cli ` +
        `example(s); found ${blocks.length}. A drop means the examples were removed ` +
        `or the extractor narrowed - either way this gate stopped checking them.`,
    );
    const diagnostics = typecheckDoc(blocks, `docs/${file}`, shape);
    assert.equal(
      diagnostics,
      null,
      `an example in docs/${file} no longer compiles against zero-migrate-cli.\n` +
        `Fix the doc (or the snippet) - do not weaken this gate.\n\n` +
        String(diagnostics),
    );
  });
}

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
