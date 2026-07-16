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

test("public root .d.ts exposes vendor DDL and omits recorder internals", async () => {
  const coreExports = exportedNamesFromDts("index.d.ts");

  const rootedVendorExports = [
    "createFunction",
    "domain",
    "dropFunction",
    "dropOwnedBy",
    "extension",
    "grant",
    "raw",
    "revoke",
    "role",
    "schema",
    "sequence",
  ];
  assert.equal(coreExports.has("table"), true, "table must be exported from zero-migrate root declarations");
  assert.equal(coreExports.has("ids"), true, "ids must be exported from zero-migrate root declarations");
  for (const name of ["IdFormats", "TypeIdOptions", "ValueFormat"]) {
    assert.equal(coreExports.has(name), true, `${name} must be exported from zero-migrate root declarations`);
  }
  for (const name of rootedVendorExports) {
    assert.equal(coreExports.has(name), true, `${name} must be exported from zero-migrate root declarations`);
  }

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
    "pg" + "Table",
  ];
  for (const name of forbiddenInternalExports) {
    assert.equal(coreExports.has(name), false, `${name} must stay out of zero-migrate root declarations`);
  }

  const indexDts = readFileSync(new URL("../dist/index.d.ts", import.meta.url), "utf8");
  assert.doesNotMatch(indexDts, /\bCreateRawViewArgs\b/);
  assert.doesNotMatch(indexDts, /\bcreateRaw\b/);

  const runtimeRoot = await import("zero-migrate");
  assert.equal(typeof runtimeRoot.ids, "object", "ids must be a root runtime namespace");
  assert.equal(typeof runtimeRoot.ids.typeId, "function", "ids.typeId must be a root runtime builder");
  for (const name of rootedVendorExports) {
    assert.equal(typeof (runtimeRoot as Record<string, unknown>)[name], "function", `${name} must be a root runtime export`);
  }
  for (const name of forbiddenInternalExports) {
    assert.equal((runtimeRoot as Record<string, unknown>)[name], undefined, `${name} must stay out of zero-migrate root runtime exports`);
  }
});
