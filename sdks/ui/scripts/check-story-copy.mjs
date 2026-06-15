#!/usr/bin/env node
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const ROOT = new URL("../src/stories/", import.meta.url);

const bannedPattern =
  /\b(?:regression|pre-fix|post-fix|review-?fix|fix\s*#\d+|slice\s*\d+|wave\s*-?\s*\d+|wave\d+|round\s+\d+|anti-pattern)\b|[🔴🟡🟠]/i;

const storyPropertyNames = new Set([
  "name",
  "story",
  "title",
  "description",
  "label",
  "eyebrow",
  "caption",
  "subtitle",
  "subTitle",
]);

const jsxAttributeNames = new Set([
  "aria-label",
  "aria-description",
  "alt",
  "label",
  "placeholder",
  "title",
  "description",
]);

async function listStories(dir) {
  const entries = await fs.readdir(dir, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listStories(full)));
    } else if (entry.name.endsWith(".stories.tsx")) {
      files.push(full);
    }
  }
  return files;
}

function propNameText(name) {
  if (ts.isIdentifier(name) || ts.isStringLiteral(name)) return name.text;
  return undefined;
}

function textFromExpression(node) {
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) {
    return node.text;
  }
  if (ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.PlusToken) {
    const left = textFromExpression(node.left);
    const right = textFromExpression(node.right);
    return left != null && right != null ? left + right : undefined;
  }
  return undefined;
}

function checkText({ failures, sourceFile, file, kind, node, text }) {
  if (!text || !bannedPattern.test(text)) return;
  const { line, character } = sourceFile.getLineAndCharacterOfPosition(
    node.getStart(sourceFile),
  );
  failures.push({
    file,
    line: line + 1,
    column: character + 1,
    kind,
    text: text.replace(/\s+/g, " ").trim(),
  });
}

function visitStory(sourceFile, file, failures) {
  function visit(node) {
    if (
      ts.isVariableStatement(node) &&
      node.modifiers?.some((modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword)
    ) {
      for (const declaration of node.declarationList.declarations) {
        if (ts.isIdentifier(declaration.name)) {
          checkText({
            failures,
            sourceFile,
            file,
            kind: "export:story",
            node: declaration.name,
            text: declaration.name.text,
          });
        }
      }
    }

    if (ts.isPropertyAssignment(node)) {
      const name = propNameText(node.name);
      if (name && storyPropertyNames.has(name)) {
        const text = textFromExpression(node.initializer);
        checkText({
          failures,
          sourceFile,
          file,
          kind: `property:${name}`,
          node,
          text,
        });
      }
    }

    if (ts.isJsxAttribute(node)) {
      const name = node.name.text;
      if (jsxAttributeNames.has(name) && node.initializer) {
        let text;
        if (ts.isStringLiteral(node.initializer)) {
          text = node.initializer.text;
        } else if (
          ts.isJsxExpression(node.initializer) &&
          node.initializer.expression
        ) {
          text = textFromExpression(node.initializer.expression);
        }
        checkText({
          failures,
          sourceFile,
          file,
          kind: `jsx:${name}`,
          node,
          text,
        });
      }
    }

    if (ts.isJsxText(node)) {
      checkText({
        failures,
        sourceFile,
        file,
        kind: "jsx:text",
        node,
        text: node.getText(sourceFile),
      });
    }

    ts.forEachChild(node, visit);
  }

  visit(sourceFile);
}

const storyDir = fileURLToPath(ROOT);
const files = await listStories(storyDir);
const failures = [];

for (const file of files) {
  const source = await fs.readFile(file, "utf8");
  const sourceFile = ts.createSourceFile(
    file,
    source,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TSX,
  );
  visitStory(sourceFile, file, failures);
}

if (failures.length > 0) {
  console.error("Story copy contains internal test/review jargon:");
  for (const failure of failures) {
    console.error(
      `${path.relative(process.cwd(), failure.file)}:${failure.line}:${failure.column} ` +
        `${failure.kind} "${failure.text}"`,
    );
  }
  process.exit(1);
}

console.log(`Story copy guard passed for ${files.length} stories.`);
