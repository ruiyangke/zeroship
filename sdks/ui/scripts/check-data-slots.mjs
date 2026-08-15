#!/usr/bin/env node
/**
 * Every element the library styles must be addressable by attribute.
 *
 * A theme targets `[data-slot="input-control"]`. An element carrying a `zs-*`
 * class but no `data-slot` is reachable only by class name, so a theme written
 * against attributes cannot touch it. The classes are on their way out; the
 * slot is what replaces them, and this is what keeps the replacement total.
 *
 * It walks the TypeScript AST rather than matching text. Three hand-rolled
 * attempts to find the enclosing JSX tag by string search all produced wrong
 * counts: `onChange={(e) => ...}` and `ref={r as Ref<T>}` each put a `>` in
 * front of the attribute, and a fixed lookahead window either bled into the
 * NEXT element's slot (reporting full coverage over nine real gaps) or stopped
 * short of a slot written before `className` (reporting 103 gaps that were not
 * there). Both readings were wrong, in opposite directions, from the same
 * script. The parser knows where a tag ends; no amount of window-tuning does.
 */
import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { classNameLiterals } from "./class-name-literals.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));
const files = execFileSync(
  "git",
  [
    "ls-files",
    "src/components/**/*.tsx",
    "src/layouts/**/*.tsx",
    "src/blocks/**/*.tsx",
  ],
  { encoding: "utf8", cwd: root },
)
  .trim()
  .split("\n")
  .filter(Boolean)
  .map((file) => path.join(root, file));

const program = ts.createProgram(files, {
  jsx: ts.JsxEmit.ReactJSX,
  target: ts.ScriptTarget.Latest,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
  strict: true,
  noEmit: true,
  skipLibCheck: true,
});
const checker = program.getTypeChecker();

/** Every JSX tag in a file that carries a zs- class or a data-slot. */
function tagsOf(file) {
  const source = program.getSourceFile(file);
  if (!source) return [];
  const rel = path.relative(root, file);
  const out = [];
  const visit = (node) => {
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      const classes = [];
      let slots = 0;
      for (const attr of node.attributes.properties) {
        if (!ts.isJsxAttribute(attr)) continue;
        const name = attr.name.getText(source);
        if (name === "data-slot") slots++;
        if (name !== "className" || !attr.initializer) continue;
        for (const literal of classNameLiterals(attr.initializer, checker)) {
          classes.push(...(literal.getText().match(/\bzs-[a-z0-9_-]+/g) ?? []));
        }
      }
      if (classes.length > 0 || slots > 0) {
        out.push({
          file: rel,
          classes,
          slots,
          line:
            source.getLineAndCharacterOfPosition(node.getStart(source)).line +
            1,
        });
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return out;
}

const tags = files.flatMap(tagsOf);
const uncovered = tags.filter((t) => t.classes.length > 0 && t.slots === 0);
const doubled = tags.filter((t) => t.slots > 1);
const covered = tags.filter((t) => t.slots === 1);

const problems = [];
for (const t of uncovered) {
  problems.push(`${t.file}:${t.line}  ${t.classes[0]} has no data-slot`);
}
// A duplicate JSX attribute parses fine and the second one silently wins. The
// sweep that added these ran more than once, so it is worth stating.
for (const t of doubled) {
  problems.push(
    `${t.file}:${t.line}  ${t.slots} data-slot attributes on one tag`,
  );
}
// A floor, so that deleting slots wholesale fails here rather than making the
// check above vacuously true.
if (covered.length < 453) {
  problems.push(
    `only ${covered.length} slotted elements; expected at least 453`,
  );
}

if (problems.length > 0) {
  console.error("data-slot coverage:\n  " + problems.join("\n  "));
  process.exit(1);
}
console.log(`data-slot coverage: ${covered.length} slotted elements, no gaps`);
