// Shared generator for the vendor attribute typings of the `zero-migrate-<dialect>`
// packages. Each package's `scripts/gen-attributes.mjs` reads its own crate's
// `attribute-vocabulary.json` and writes `src/generated/attributes.ts` through
// `generate`. One copy is what keeps the packages agreeing on how a declared shape
// becomes a TypeScript type.
//
// This module is vendor-blind: the dialect, the keys, the shapes and the documentation
// all come from the vocabulary document. Adding a backend adds a vocabulary file and a
// thin wrapper script, and this module is unchanged.
//
// The emitted module augments the neutral `@zeroship/migrate` package's
// `VendorAttributeNamespaces` and `VendorIndexAttributeNamespaces` interfaces. Importing
// a vendor package is what makes its namespace key typecheck on the authoring surface.

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

/** The vocabulary wire shape this module understands. */
const VOCABULARY_FORMAT_VERSION = 1;

/**
 * The paragraph wrap width, in characters of comment text. The ` * ` prefix the JSDoc
 * renderer adds sits outside this budget.
 */
const WRAP_WIDTH = 84;

/**
 * The authoring scopes, in emission order. Each scope names the op kinds whose attributes
 * it carries, the interface it declares them on, and the `@zeroship/migrate` interface its
 * namespace key augments.
 */
const SCOPES = [
  {
    label: "CreateTable",
    ops: ["createTable", "createPartition", "setTableOptions"],
    namespace: "VendorAttributeNamespaces",
  },
  {
    label: "CreateIndex",
    ops: ["createIndex"],
    namespace: "VendorIndexAttributeNamespaces",
  },
];

/** The DialectId rule from `zeroship_migrate_ir::dialect`: lowercase `[a-z][a-z0-9_]*`. */
const DIALECT_ID = /^[a-z][a-z0-9_]*$/;

/** The exact-decimal spelling an integer bound must carry. */
const EXACT_INTEGER = /^-?\d+$/;

/** Upper-case the first character, leaving the rest of the dialect id intact. */
function pascalCase(value) {
  return value.charAt(0).toUpperCase() + value.slice(1);
}

/** Break one paragraph into lines of at most {@link WRAP_WIDTH} characters, greedily. */
function wrapParagraph(paragraph) {
  const words = paragraph.split(/\s+/).filter((word) => word !== "");
  const lines = [];
  let line = "";
  for (const word of words) {
    if (line === "") line = word;
    else if (line.length + 1 + word.length <= WRAP_WIDTH) line += ` ${word}`;
    else {
      lines.push(line);
      line = word;
    }
  }
  if (line !== "") lines.push(line);
  return lines;
}

/** Render a field's documentation as an indented JSDoc block. */
function renderDocBlock(text) {
  const lines = [];
  for (const paragraph of text.split("\n\n")) {
    if (lines.length > 0) lines.push("");
    for (const line of wrapParagraph(paragraph)) lines.push(line);
  }
  return lines.map((line) => (line === "" ? "     *" : `     * ${line}`)).join("\n");
}

/** Render a declared shape as the TypeScript type of its field. */
function renderShape(key, shape) {
  switch (shape.kind) {
    case "bool":
      return "boolean";
    case "int":
      return "number";
    case "text":
      return "string";
    case "enum":
      return shape.variants.map((variant) => JSON.stringify(variant)).join(" | ");
    default:
      throw new Error(`attribute ${key} has unknown shape kind ${JSON.stringify(shape.kind)}`);
  }
}

/** The documented shape of an integer field, rendered as a second paragraph. */
function rangeParagraph(shape) {
  return `Accepted range: ${shape.min}..=${shape.max} (enforced when the migration is planned, not by this type).`;
}

/** The header every generated file carries. */
function banner(dialect) {
  return `// GENERATED FILE - DO NOT EDIT.
//
// Source: the \`attribute-vocabulary.json\` exported by this backend's Rust crate from
// its own \`static DEFS\`. Regenerate with:
//
//     cargo test -p zeroship-migrate-${dialect} --test integration -- --ignored update_attribute_vocabulary
//     node packages/zero-migrate-${dialect}/scripts/gen-attributes.mjs
//
// A drift test asserts this file matches the artifact, so an edit here is reverted by
// the next regeneration rather than silently kept.
//
// This module augments the neutral \`@zeroship/migrate\` package's \`VendorAttributeNamespaces\`
// interface. Importing this package is what makes \`${dialect}: { ... }\` typecheck on the
// authoring surface; without it the key is a type error. The namespace key is the
// backend's DIALECT ID - there is no hand-picked alias anywhere in the chain.`;
}

/** Render the whole generated module. */
function render(dialect, attributes) {
  const grouped = new Map(SCOPES.map((scope) => [scope.label, []]));
  for (const def of attributes) {
    grouped.get(scopeOf(def.key, def.ops).label).push(def);
  }

  const prefix = pascalCase(dialect);
  const blocks = [];
  for (const scope of SCOPES) {
    const defs = grouped.get(scope.label);
    if (defs.length === 0) continue;

    const interfaceName = `${prefix}${scope.label}Attributes`;
    const fields = defs.map((def) => {
      const leaf = def.key.slice(dialect.length + 1);
      let text = def.docs;
      if (def.shape.kind === "int") text += `\n\n${rangeParagraph(def.shape)}`;
      return `    /**\n${renderDocBlock(text)}\n     */\n    ${leaf}?: ${renderShape(def.key, def.shape)};`;
    });

    blocks.push(`  /** ${scope.label}-level options this backend accepts. */
  interface ${interfaceName} {
${fields.join("\n\n")}
  }

  interface ${scope.namespace} {
    /**
     * ${scope.label} options specific to the \`${dialect}\` backend.
     *
     * Present because this package is installed. Every field is optional, and an object
     * that also carries other backends' options stays portable to all of them.
     */
    ${dialect}?: ${interfaceName};
  }`);
  }

  return `${banner(dialect)}

import type {} from "@zeroship/migrate";

declare module "@zeroship/migrate" {
${blocks.join("\n\n")}
}

export {};
`;
}

/** Throw a clear error naming the vocabulary file and the defect. */
function fail(path, message) {
  throw new Error(`${path}: ${message}`);
}

function isPlainObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The single scope an attribute belongs to, refusing one that names none or several. */
function scopeOf(key, ops) {
  const matches = SCOPES.filter((scope) => ops.some((op) => scope.ops.includes(op)));
  if (matches.length !== 1) {
    const named = matches.map((scope) => scope.label).join(", ");
    throw new Error(
      `attribute ${key} must belong to exactly one scope; it names ${named === "" ? "none" : named}`,
    );
  }
  return matches[0];
}

function validateShape(path, key, shape) {
  if (!isPlainObject(shape)) fail(path, `attribute ${key} must carry a shape object`);
  switch (shape.kind) {
    case "bool":
    case "text":
      return;
    case "int":
      if (typeof shape.min !== "string" || !EXACT_INTEGER.test(shape.min)) {
        fail(path, `attribute ${key} must carry "min" as an exact decimal string`);
      }
      if (typeof shape.max !== "string" || !EXACT_INTEGER.test(shape.max)) {
        fail(path, `attribute ${key} must carry "max" as an exact decimal string`);
      }
      return;
    case "enum":
      if (
        !Array.isArray(shape.variants) ||
        shape.variants.length === 0 ||
        !shape.variants.every((variant) => typeof variant === "string")
      ) {
        fail(path, `attribute ${key} must carry a non-empty "variants" array of strings`);
      }
      return;
    default:
      fail(path, `attribute ${key} has unknown shape kind ${JSON.stringify(shape.kind)}`);
  }
}

function validateAttribute(path, dialect, def) {
  if (!isPlainObject(def)) fail(path, `every attribute must be an object, got ${JSON.stringify(def)}`);
  const { key, ops, shape, docs } = def;
  if (typeof key !== "string" || !key.startsWith(`${dialect}.`) || key.length <= dialect.length + 1) {
    fail(path, `attribute key ${JSON.stringify(key)} must be prefixed with "${dialect}."`);
  }
  if (!Array.isArray(ops) || ops.length === 0) {
    fail(path, `attribute ${key} must declare the ops it is legal on`);
  }
  for (const op of ops) {
    if (!SCOPES.some((scope) => scope.ops.includes(op))) {
      fail(path, `attribute ${key} names unknown op ${JSON.stringify(op)}`);
    }
  }
  if (typeof docs !== "string" || docs.trim() === "") {
    fail(path, `attribute ${key} must carry non-empty docs`);
  }
  validateShape(path, key, shape);
  return scopeOf(key, ops);
}

function validateDocument(path, doc) {
  if (!isPlainObject(doc)) fail(path, "the vocabulary must be a JSON object");
  if (doc.version !== VOCABULARY_FORMAT_VERSION) {
    fail(
      path,
      `unsupported vocabulary version ${JSON.stringify(doc.version)}; this generator understands ${VOCABULARY_FORMAT_VERSION}`,
    );
  }
  if (typeof doc.dialect !== "string" || !DIALECT_ID.test(doc.dialect)) {
    fail(path, `"dialect" must be a DialectId (lowercase [a-z][a-z0-9_]*), got ${JSON.stringify(doc.dialect)}`);
  }
  if (!Array.isArray(doc.attributes) || doc.attributes.length === 0) {
    fail(path, "the vocabulary declares no attributes; an empty typings file reads as \"this backend has no knobs\"");
  }

  const seen = new Set();
  for (const def of doc.attributes) {
    const scope = validateAttribute(path, doc.dialect, def);
    const identity = `${def.key}\0${scope.label}`;
    if (seen.has(identity)) {
      fail(path, `attribute ${def.key} is declared twice for ${scope.label}`);
    }
    seen.add(identity);
  }
}

/**
 * Read a vocabulary document and write its TypeScript typings to `outPath`.
 *
 * @param {{ vocabularyPath: string, outPath: string }} options
 * @returns {Promise<{ dialect: string, count: number, byScope: Record<string, number> }>}
 */
export async function generate({ vocabularyPath, outPath }) {
  const raw = await readFile(vocabularyPath, "utf8");
  let doc;
  try {
    doc = JSON.parse(raw);
  } catch (error) {
    throw new Error(`${vocabularyPath}: not valid JSON: ${error.message}`);
  }

  validateDocument(vocabularyPath, doc);

  const byScope = {};
  for (const scope of SCOPES) byScope[scope.label] = 0;
  for (const def of doc.attributes) {
    byScope[scopeOf(def.key, def.ops).label] += 1;
  }

  await mkdir(dirname(outPath), { recursive: true });
  await writeFile(outPath, render(doc.dialect, doc.attributes), "utf8");

  return { dialect: doc.dialect, count: doc.attributes.length, byScope };
}
