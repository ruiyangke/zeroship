// Generate the dialect-neutral IR *enum* token types from the engine's
// single-source-of-truth schema `crates/zeroship-migrate/ir-envelope.schema.json` via
// `json-schema-to-typescript`.
//
// SCOPE — codegen covers the CLOSED STRING-ENUM defs only (BinaryOp, UnaryOp,
// ScalarFn, SynthFn, CastTarget, IndexMethod, CmpOp, OnUnmet,
// OnlinePhase). The RECURSIVE structural defs (`Expr`/`Op`/`ColType`/
// `IrConstraint`/`MigrationIr`/…) are a self-recursive `oneOf` AST that
// json-schema-to-typescript v15 CANNOT express (it inlines the `$ref` cycle and
// overflows the stack), so those are HAND-AUTHORED in `src/generated/ir.ts` (the
// task's "manual types for any serde shape codegen cannot express"). The type
// manifest declares which closed enums are generated and which remain handwritten.
//
// These types are ERGONOMICS for an advanced caller; the golden IR-envelope corpus
// + the `Checksum::of_ir` round-trip (in `crates/zeroship-migrate/tests`)
// remain the contract source of truth. Regenerate with:
//
//   pnpm --filter @zeroship/migrate gen:ir-types
//
// then commit the regenerated `src/generated/enums.ts`.

import { compile } from "json-schema-to-typescript";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const schemaPath = resolve(here, "../../../crates/zeroship-migrate/ir-envelope.schema.json");
const manifestPath = resolve(here, "ir-type-manifest.json");
// The output path defaults to the committed enums.ts. The generated-artifact
// check overrides it via `GEN_IR_OUT` and compares the result with that output.
const outPath = process.env.GEN_IR_OUT
  ? resolve(process.env.GEN_IR_OUT)
  : resolve(here, "../src/generated/enums.ts");

const banner = `/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zeroship-migrate/ir-envelope.schema.json (the engine's single-source-of-
// truth IR schema). Regenerate with: pnpm --filter @zeroship/migrate gen:ir-types
//
// Covers the CLOSED STRING-ENUM IR defs only; the recursive structural types live
// (hand-authored) in ./ir.ts. These are ERGONOMICS; the golden IR-envelope corpus is
// the contract.
`;

const raw = await readFile(schemaPath, "utf8");
const schema = JSON.parse(raw);
const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
const enumDefs = manifest.generated;

// The engine schema's doc descriptions carry the `@zeroship/migrate` brand directly
// (the Rust `#[doc]` comments name the standalone `@zeroship/migrate` package),
// so the emitted TS doc strings need no specifier rewrite —
// this is an identity pass-through kept as a seam for any future doc-only remap.
function sanitizeDoc(node) {
  return node;
}

const parts = [];
for (const name of enumDefs) {
  const def = sanitizeDoc(schema.$defs[name]);
  if (!def) throw new Error(`enum def ${name} missing from schema`);
  const ts = await compile({ ...def, title: name }, name, {
    bannerComment: "",
    additionalProperties: false,
  });
  parts.push(ts.trim());
}

await mkdir(dirname(outPath), { recursive: true });
await writeFile(outPath, banner + "\n" + parts.join("\n\n") + "\n", "utf8");
console.log(`wrote ${outPath}`);
