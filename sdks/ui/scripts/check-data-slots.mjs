#!/usr/bin/env node
/**
 * Every element the library styles must be addressable by attribute.
 *
 * A theme targets `[data-slot="input-control"]`. An element carrying a `zs-*`
 * class but no `data-slot` is reachable only by class name, so a theme written
 * against attributes cannot touch it. This includes interpolated templates
 * whose block name is dynamic, such as `zs-${base}-field`. The classes are on
 * their way out; the slot is what replaces them, and this is what keeps the
 * replacement total.
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

/**
 * Scan everything under src/ EXCEPT what is listed here.
 *
 * This used to name the directories to include (components, layouts, blocks)
 * and missed real code three times: `dir/**\/*.tsx` skipped files sitting
 * directly in a directory, the list omitted src/sections entirely, and
 * src/theme.tsx was invisible for both reasons. src/sections holds ten
 * EXPORTED components (Hero, Footer, PricingTable, ...) whose classes were
 * nearly deleted as orphaned theme CSS, because nothing scanned them.
 *
 * An inclusion list fails silently: a directory added later simply never
 * appears. Excluding instead fails loudly, as unexpected gaps.
 */
const isExcluded = (file) =>
  file.startsWith("src/stories/") || // consumer demos: plain markup, no slot contract
  file.endsWith(".stories.tsx") ||
  file.endsWith("type-tests.tsx");

const files = execFileSync("git", ["ls-files", "src"], {
  encoding: "utf8",
  cwd: root,
})
  .trim()
  .split("\n")
  .filter((file) => file.endsWith(".tsx") && !isExcluded(file))
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

/** String-literal members of a type, or null if it is not such a union. */
function stringLiteralsOf(type) {
  const parts = type.isUnion() ? type.types : [type];
  const out = [];
  for (const t of parts) {
    if (t.isStringLiteral()) out.push(t.value);
    else if (t.flags & ts.TypeFlags.Undefined) continue;
    else return null;
  }
  return out.length > 0 ? out : null;
}

/**
 * Every concrete string a template can produce, or null if any interpolated
 * part is not a string-literal union.
 */
function expandTemplate(node, checker) {
  let names = [node.head.text];
  for (const span of node.templateSpans) {
    const parts = stringLiteralsOf(checker.getTypeAtLocation(span.expression));
    if (!parts) return null;
    const tail = span.literal.text;
    names = names.flatMap((prefix) => parts.map((p) => prefix + p + tail));
  }
  return names;
}

/**
 * The concrete name(s) a `data-slot` can take, or null if it cannot be pinned.
 *
 * Three shapes exist and only the first is greppable, which is why a text
 * search finds 476 names where the AST finds far more:
 *
 *   data-slot="button-spinner"     literal
 *   data-slot={dataSlot}           a prop with a DEFAULT
 *   data-slot={`${base}-field`}    interpolated over a string-literal union
 *
 * The second is the subtle one. `"data-slot"?: string` means the checker
 * reports the type as `string`, not `"button"` -- the value lives in the
 * destructuring default (`"data-slot": dataSlot = "button"`), so this reads the
 * binding's initializer rather than its type.
 */
function slotNamesOf(initializer, checker, source) {
  if (!initializer) return null;
  if (ts.isStringLiteral(initializer)) return [initializer.text];
  if (!ts.isJsxExpression(initializer) || !initializer.expression) return null;
  const expr = initializer.expression;

  if (ts.isStringLiteral(expr) || ts.isNoSubstitutionTemplateLiteral(expr)) {
    return [expr.text];
  }

  if (ts.isTemplateExpression(expr)) {
    let names = [expr.head.text];
    for (const span of expr.templateSpans) {
      const parts = stringLiteralsOf(checker.getTypeAtLocation(span.expression));
      if (!parts) return null;
      const tail = span.literal.text;
      names = names.flatMap((prefix) => parts.map((p) => prefix + p + tail));
    }
    return names;
  }

  if (ts.isIdentifier(expr)) {
    const symbol = checker.getSymbolAtLocation(expr);
    for (const decl of symbol?.declarations ?? []) {
      // `{ "data-slot": dataSlot = "button" }` -- the default, not the type.
      if (ts.isBindingElement(decl) && decl.initializer) {
        if (ts.isStringLiteral(decl.initializer)) return [decl.initializer.text];
        if (ts.isNoSubstitutionTemplateLiteral(decl.initializer)) {
          return [decl.initializer.text];
        }
      }
      if (ts.isVariableDeclaration(decl) && decl.initializer) {
        if (ts.isStringLiteral(decl.initializer)) return [decl.initializer.text];
      }
    }
    const fromType = stringLiteralsOf(checker.getTypeAtLocation(expr));
    if (fromType) return fromType;
  }

  void source;
  return null;
}

/** Every JSX tag in a file that carries a zs- class or a data-slot. */
function tagsOf(file) {
  const source = program.getSourceFile(file);
  if (!source) return [];
  const rel = path.relative(root, file);
  const out = [];
  const visit = (node) => {
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      const classes = [];
      const names = [];
      const unresolved = [];
      let slots = 0;
      for (const attr of node.attributes.properties) {
        if (!ts.isJsxAttribute(attr)) continue;
        const name = attr.name.getText(source);
        if (name === "data-slot") {
          slots++;
          const resolved = slotNamesOf(attr.initializer, checker, source);
          if (resolved) names.push(...resolved);
          else unresolved.push(attr.getText(source).replace(/\s+/g, " "));
        }
        if (name !== "className" || !attr.initializer) continue;
        for (const literal of classNameLiterals(attr.initializer, checker)) {
          classes.push(...(literal.getText().match(/\bzs-[a-z0-9_-]+/g) ?? []));
          if (
            ts.isTemplateExpression(literal) &&
            literal.head.text.startsWith("zs-")
          ) {
            // Expand the interpolation into concrete class names when the
            // interpolated parts are string-literal unions. SelectionRow
            // builds `zs-${base}-field`, so the real classes are
            // zs-checkbox-field / zs-radio-field / zs-switch-field -- and the
            // theme styles them by those names. Keeping only the raw template
            // text leaves those selectors with nothing to match.
            const expanded = expandTemplate(literal, checker);
            classes.push(...(expanded ?? [literal.getText(source)]));
          }
        }
      }
      if (classes.length > 0 || slots > 0) {
        out.push({
          file: rel,
          classes,
          slots,
          names,
          unresolved,
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

// `--list-slots` prints the resolved names, one per line, and nothing else, so
// it can be piped. It is the authority the theme translation reads: a selector
// looks orphaned precisely when its slot name is missing from this list, so an
// unresolvable value must fail loudly rather than be quietly dropped.
if (process.argv.includes("--list-slots")) {
  const stuck = tags.flatMap((t) =>
    t.unresolved.map((u) => `${t.file}:${t.line}  ${u}`),
  );
  if (stuck.length > 0) {
    console.error("unresolvable data-slot values:\n  " + stuck.join("\n  "));
    process.exit(1);
  }
  for (const name of [...new Set(tags.flatMap((t) => t.names))].sort()) {
    console.log(name);
  }
  process.exit(0);
}

// `--map-classes` prints `class<TAB>slot` for every classed element, which is
// the table the theme translation needs.
//
// Deriving the slot from the class name does NOT work: they drifted. The <th>
// carries zs-data-table__th but slot "data-table-column-header", and
// PricingTable's root is zs-pricing with slot "pricing-table". Sixteen
// selectors disagree that way, and a derived name would target nothing --
// silently, because a selector matching no element still builds and ships.
//
// Every hand-rolled attempt to recover this pairing by searching lines around
// the class was wrong: data-slot can sit before OR after className in the same
// tag, so a backwards scan reported six elements as having no slot when the
// AST says every one of them has one. The parser knows which attributes belong
// to which tag; line proximity does not.
if (process.argv.includes("--map-classes")) {
  const pairs = new Set();
  for (const t of tags) {
    for (const c of t.classes) {
      for (const n of t.names) pairs.add(`${c}\t${n}`);
    }
  }
  for (const p of [...pairs].sort()) console.log(p);
  process.exit(0);
}

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
if (covered.length < 564) {
  problems.push(
    `only ${covered.length} slotted elements; expected at least 564`,
  );
}

console.log(`data-slot coverage: scanned ${files.length} files`);
if (problems.length > 0) {
  console.error("data-slot coverage:\n  " + problems.join("\n  "));
  process.exit(1);
}
console.log(`data-slot coverage: ${covered.length} slotted elements, no gaps`);
