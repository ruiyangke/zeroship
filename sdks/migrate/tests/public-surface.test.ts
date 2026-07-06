import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import * as ts from "typescript";

function exportedNamesFromDts(fileName: string): Set<string> {
  const sourceText = readFileSync(new URL(`../dist/${fileName}`, import.meta.url), "utf8");
  const source = ts.createSourceFile(fileName, sourceText, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  const names = new Set<string>();

  function hasExportModifier(node: ts.Node): boolean {
    return Boolean(ts.canHaveModifiers(node) && ts.getModifiers(node)?.some((mod) => mod.kind === ts.SyntaxKind.ExportKeyword));
  }

  for (const statement of source.statements) {
    if (ts.isExportDeclaration(statement)) {
      const clause = statement.exportClause;
      assert.ok(clause, `${fileName} must use named exports so the public surface can be linted`);
      if (ts.isNamedExports(clause)) {
        for (const element of clause.elements) names.add(element.name.text);
      } else {
        names.add(clause.name.text);
      }
      continue;
    }

    if (!hasExportModifier(statement)) continue;
    if (
      ts.isFunctionDeclaration(statement) ||
      ts.isClassDeclaration(statement) ||
      ts.isInterfaceDeclaration(statement) ||
      ts.isTypeAliasDeclaration(statement) ||
      ts.isEnumDeclaration(statement) ||
      ts.isModuleDeclaration(statement)
    ) {
      if (statement.name) names.add(statement.name.text);
      continue;
    }

    if (ts.isVariableStatement(statement)) {
      for (const declaration of statement.declarationList.declarations) {
        if (ts.isIdentifier(declaration.name)) names.add(declaration.name.text);
      }
    }
  }

  return names;
}

test("public root .d.ts does not leak pg-only or recorder internals", async () => {
  const coreExports = exportedNamesFromDts("index.d.ts");
  const pgExports = exportedNamesFromDts("pg.d.ts");

  const rootSharedPgTypes = new Set(["PgExtractField"]);
  const pgLeaks = [...pgExports].filter((name) => coreExports.has(name) && !rootSharedPgTypes.has(name)).sort();
  assert.deepEqual(pgLeaks, [], "pg-only symbols belong to @zeroship/migrate/pg, not the package root");

  const forbiddenInternalExports = [
    "__begin",
    "__drain",
    "__pgDomain",
    "__pgPush",
    "__pgResolveExpr",
    "__pgSequence",
    "cAgg",
    "cCase",
    "opProducers",
    "opProducerRegistry",
  ];
  for (const name of forbiddenInternalExports) {
    assert.equal(coreExports.has(name), false, `${name} must stay out of @zeroship/migrate root declarations`);
  }

  const indexDts = readFileSync(new URL("../dist/index.d.ts", import.meta.url), "utf8");
  assert.doesNotMatch(indexDts, /\bCreateRawViewArgs\b/);
  assert.doesNotMatch(indexDts, /\bcreateRaw\b/);

  const runtimeRoot = await import("@zeroship/migrate");
  for (const name of [...pgExports, ...forbiddenInternalExports]) {
    assert.equal((runtimeRoot as Record<string, unknown>)[name], undefined, `${name} must stay out of @zeroship/migrate root runtime exports`);
  }
});
