#!/usr/bin/env node
/**
 * Map each `zs-block--modifier` class to the data-attribute that carries it.
 *
 * Guessing from the modifier NAME does not work: `danger` is data-intent on one
 * component and data-status on another. Guessing from the BLOCK does not work
 * either -- `zs-input--*` is fed by both `variant` and `size`, so the block
 * alone cannot say whether `.zs-input--sm` is a size or a variant.
 *
 * The component states the answer twice over, and this reads both:
 *
 *   className={classnames("zs-input", `zs-input--${variant}`, ...)}
 *   data-variant={variant}          <- same expression, so the class is a variant
 *   variant?: InputVariant          <- "outline" | "filled" | "plain"
 *
 * Pairing class to attribute by expression gives the attribute; asking the type
 * checker for that expression's type gives the values it can take. Together
 * they place every modifier exactly, with no name-based inference.
 *
 * Emits JSON: { "input": { "outline": "variant", "sm": "size", ... }, ... }
 */
import { execFileSync } from "node:child_process";
import path from "node:path";
import ts from "typescript";
import {
  classNameLiterals,
  objectProperties,
} from "./class-name-literals.mjs";

const root = new URL("../", import.meta.url).pathname;
const files = execFileSync(
  "git",
  // Scan all of src/ and exclude, rather than listing what to include. The
  // inclusion list missed src/sections (ten exported components) and
  // src/theme.tsx, and a missing directory produces no output at all rather
  // than an error. See the same note in check-data-slots.mjs.
  ["ls-files", "src"],
  { encoding: "utf8", cwd: root },
)
  .trim()
  .split("\n")
  .filter(
    (file) =>
      file.endsWith(".tsx") &&
      !file.startsWith("src/stories/") &&
      !file.endsWith(".stories.tsx") &&
      !file.endsWith("type-tests.tsx"),
  )
  .map((f) => path.join(root, f));

const program = ts.createProgram(files, {
  jsx: ts.JsxEmit.ReactJSX,
  target: ts.ScriptTarget.Latest,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
  strict: true,
  noEmit: true,
  skipLibCheck: true,
});
const checker = program.getTypeChecker();

/** block -> value -> attribute */
const map = new Map();
const problems = [];

/**
 * Blocks whose attribute comes from the underlying Base UI component rather
 * than from our own JSX, so pairing by expression finds nothing.
 *
 * Both forward `orientation={orientation}` to a Base UI root that emits
 * `data-orientation` itself. Confirmed by rendering, not by reading Base UI's
 * source: renderToStaticMarkup of each with orientation="vertical" gives
 * `data-orientation="vertical"` alongside our own `data-slot`. A theme can
 * target these today; nothing needs adding.
 */
const SUPPLIED_BY_BASE_UI = {
  menubar: { expr: "orientation", attr: "orientation" },
  toolbar: { expr: "orientation", attr: "orientation" },
};

function propertyName(name) {
  if (
    ts.isIdentifier(name) ||
    ts.isStringLiteral(name) ||
    ts.isNoSubstitutionTemplateLiteral(name)
  ) {
    return name.text;
  }
  return null;
}

/** String-literal members of a type, or null if it is not such a union. */
function literalsOf(type) {
  const parts = type.isUnion() ? type.types : [type];
  const out = [];
  for (const t of parts) {
    if (t.isStringLiteral()) out.push(t.value);
    else if (t.flags & ts.TypeFlags.Undefined) continue;
    else return null; // `string`, or something else: not enumerable
  }
  return out.length > 0 ? out : null;
}

/** Members of an all-string-literal union, or null otherwise. */
function unionLiteralsOf(type) {
  if (!type.isUnion() || type.types.some((t) => !t.isStringLiteral())) {
    return null;
  }
  return type.types.map((t) => t.value);
}

for (const file of files) {
  const source = program.getSourceFile(file);
  if (!source) continue;
  const rel = path.relative(root, file);

  const visit = (node) => {
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      const byExpr = new Map(); // expression text -> { attr, expr } for data-*
      const anyExpr = new Map(); // expression text -> expr, for every attribute
      let classAttr = null;
      for (const attr of node.attributes.properties) {
        if (ts.isJsxSpreadAttribute(attr)) {
          for (const property of objectProperties(attr.expression, checker)) {
            const name = propertyName(property.name);
            if (!name) continue;
            const text = property.initializer.getText().trim();
            anyExpr.set(text, property.initializer);
            if (name.startsWith("data-") && name !== "data-slot") {
              byExpr.set(text, {
                attr: name.slice("data-".length),
                expr: property.initializer,
              });
            }
          }
          continue;
        }
        if (!ts.isJsxAttribute(attr) || !attr.initializer) continue;
        const name = attr.name.getText(source);
        if (name === "className") {
          classAttr = attr.initializer;
          continue;
        }
        if (
          !ts.isJsxExpression(attr.initializer) ||
          !attr.initializer.expression
        )
          continue;
        const text = attr.initializer.expression.getText(source).trim();
        anyExpr.set(text, attr.initializer.expression);
        if (name.startsWith("data-") && name !== "data-slot") {
          byExpr.set(text, {
            attr: name.slice("data-".length),
            expr: attr.initializer.expression,
          });
        }
      }
      if (!classAttr) return ts.forEachChild(node, visit);

      for (const literal of classNameLiterals(classAttr, checker)) {
        for (const m of literal
          .getText()
          .matchAll(
            /`zs-(?:([a-z0-9-]+?)|\$\{([^}]+)\}([a-z0-9-]*))--\$\{([^}]+)\}`/g,
          )) {
          const [, literalBlock, rawBlockExpr, blockTail = "", rawExpr] = m;
          const blockLabel =
            literalBlock ?? `\${${rawBlockExpr.trim()}}${blockTail}`;
          let blocks = literalBlock ? [literalBlock] : null;
          if (!blocks) {
            const blockExpr = ts.isTemplateExpression(literal)
              ? literal.templateSpans[0]?.expression
              : null;
            const blockValues = blockExpr
              ? unionLiteralsOf(checker.getTypeAtLocation(blockExpr))
              : null;
            if (!blockValues) {
              problems.push(
                `${rel}: zs-${blockLabel}--\${${rawExpr.trim()}} has a block expression that is not a string-literal union`,
              );
              continue;
            }
            blocks = blockValues.map((value) => `${value}${blockTail}`);
          }
          const expr = rawExpr.trim();
          let hit = byExpr.get(expr);
          if (!hit && literalBlock) {
            const external = SUPPLIED_BY_BASE_UI[literalBlock];
            if (external && external.expr === expr && anyExpr.has(expr)) {
              // Values come from the expression as usual; only the attribute
              // name comes from Base UI rather than from a data-* on this tag.
              hit = { attr: external.attr, expr: anyExpr.get(expr) };
            }
          }
          if (!hit || !hit.expr) {
            problems.push(
              `${rel}: zs-${blockLabel}--\${${expr}} has no data-* fed by the same expression`,
            );
            continue;
          }
          const values = literalsOf(checker.getTypeAtLocation(hit.expr));
          if (!values) {
            problems.push(
              `${rel}: zs-${blockLabel}--\${${rawExpr.trim()}} is not a string-literal union`,
            );
            continue;
          }
          for (const block of blocks) {
            if (!map.has(block)) map.set(block, new Map());
            const byValue = map.get(block);
            for (const v of values) {
              const prev = byValue.get(v);
              if (prev && prev !== hit.attr) {
                problems.push(
                  `${rel}: zs-${block}--${v} could be data-${prev} or data-${hit.attr}`,
                );
              }
              byValue.set(v, hit.attr);
            }
          }
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
}

const out = {};
for (const [block, byValue] of [...map].sort()) {
  out[block] = Object.fromEntries([...byValue].sort());
}
console.error(`slot attribute map: scanned ${files.length} files`);
console.log(JSON.stringify(out, null, 2));
if (problems.length > 0) {
  console.error("\nunresolved:\n  " + [...new Set(problems)].join("\n  "));
  process.exit(1);
}
