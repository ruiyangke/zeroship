// Generate the dialect-neutral IR *enum* token types from the engine's
// single-source-of-truth schema `crates/zeroship-migrate/op-ir.schema.json` via
// `json-schema-to-typescript`.
//
// SCOPE — codegen covers the CLOSED STRING-ENUM defs only (BinaryOp, UnaryOp,
// ScalarFn, SynthFn, CastTarget, IndexMethod, SynthDefaultFn, CmpOp, OnUnmet,
// OnlinePhase). The RECURSIVE structural defs (`Expr`/`Op`/`ColType`/
// `IrConstraint`/`MigrationIr`/…) are a self-recursive `oneOf` AST that
// json-schema-to-typescript v15 CANNOT express (it inlines the `$ref` cycle and
// overflows the stack), so those are HAND-AUTHORED in `src/generated/ir.ts` (the
// task's "manual types for any serde shape codegen cannot express"). A drift test
// (`tests/ir-types-drift.test.ts`) pins both the generated enums AND the manual
// structural tokens against the schema, so the manual file cannot silently drift.
//
// These types are ERGONOMICS for an advanced caller; the golden `.ir.json` corpus
// + the `Checksum::of_ir` round-trip (in `crates/zeroship-migrate-js/tests`)
// remain the contract source of truth (§4.3 / PR3). Regenerate with:
//
//   pnpm --filter @zeroship/migrate gen:ir-types
//
// then commit the regenerated `src/generated/enums.ts`.

import { compile } from "json-schema-to-typescript";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const schemaPath = resolve(here, "../../../crates/zeroship-migrate/op-ir.schema.json");
// The output path defaults to the committed enums.ts, but the freshness CI gate
// (`tests/ir-types-drift.test.ts`) overrides it via `GEN_IR_OUT` to regenerate
// into a temp file and byte-compare against the committed copy — the "regenerate
// + diff" gate (PR10 review F4) without a shell `git diff`.
const outPath = process.env.GEN_IR_OUT
  ? resolve(process.env.GEN_IR_OUT)
  : resolve(here, "../src/generated/enums.ts");

// The closed STRING-ENUM defs (every `oneOf` branch is a `const` string). These
// have no `$ref` and are codegen-safe.
const ENUM_DEFS = [
  "BinaryOp",
  "UnaryOp",
  "ScalarFn",
  "SynthFn",
  "SynthDefaultFn",
  "CastTarget",
  "IndexMethod",
  "CmpOp",
  "OnUnmet",
  "OnlinePhase",
  // **PR10** — the closed 2-token existence-guard modifier (`ifNotExists`/
  // `ifExists`). A closed string-enum like the others, so it is generated here and
  // consumed by the hand-authored `ir.ts` `Op` variants.
  "ExistenceGuard",
  // **C1** — the closed FK referential-action lexicon (`cascade`/`restrict`/
  // `setNull`/`setDefault`/`noAction`). A closed string-enum; consumed by the
  // hand-authored `ir.ts` `IrConstraintKind` fk variant.
  "RefAction",
  // §B — closed exclusion-constraint facets.
  "ExclusionMethod",
  "ExclusionOperator",
  // §A2 — closed trigger facet/action body tokens.
  "TriggerTiming",
  "TriggerEvent",
  "ForEach",
  "RaiseLevel",
  // §A1/§3.1 — closed SelectAst facets for structured view bodies.
  "JoinKind",
  "OrderDir",
  // Vendor op closed facets used by the hand-authored full Op mirror.
  "Privilege",
  "PolicyCmd",
  "FuncArgMode",
  "FuncLanguage",
  "FuncVolatility",
  "TableStrictness",
];

const banner = `/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zeroship-migrate/op-ir.schema.json (the engine's single-source-of-
// truth IR schema). Regenerate with: pnpm --filter @zeroship/migrate gen:ir-types
//
// Covers the CLOSED STRING-ENUM IR defs only; the recursive structural types live
// (hand-authored) in ./ir.ts. These are ERGONOMICS; the golden .ir.json corpus is
// the contract (§4.3 / PR3).
`;

const raw = await readFile(schemaPath, "utf8");
const schema = JSON.parse(raw);

const parts = [];
for (const name of ENUM_DEFS) {
  const def = schema.$defs[name];
  if (!def) throw new Error(`enum def ${name} missing from schema`);
  const ts = await compile({ ...def, title: name }, name, {
    bannerComment: "",
    additionalProperties: false,
  });
  parts.push(ts.trim());
}

await mkdir(dirname(outPath), { recursive: true });
await writeFile(outPath, banner + "\n" + parts.join("\n\n") + "\n", "utf8");
console.log(`wrote ${outPath} (${ENUM_DEFS.length} enum types)`);
