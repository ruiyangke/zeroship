#!/usr/bin/env node
// schema/codegen.mjs
//
// Generate the TWO readers of `zeroship.jsonc` from `schema/project-v1.json`,
// which is the single source of truth.
//
//   node schema/codegen.mjs            # write both generated files
//   node schema/codegen.mjs --check    # regenerate in memory, diff, exit 1 on drift
//
// WHAT IS ASYMMETRIC, ON PURPOSE. The TypeScript file carries
// every `default` from the schema, because the Vite plugin must work with
// `zeroship()` and NO file at all. The Rust file carries NONE: a key the CLI
// reads and the file omits is an error naming the key. That is what makes it
// impossible for the two sides to disagree about a value -- there is only one
// place in the world that holds it.
//
// NOT A GENERAL JSON-SCHEMA COMPILER. It understands exactly the constructs
// `project-v1.json` uses: object/string/boolean/array-of-string, `enum`,
// `pattern`, `default`, `required`, `additionalProperties: false`, one level of
// `$ref` into `$defs`, and the `x-cli-read` / `x-required-members` /
// `x-forbidden-key-names` markers. Anything else in the schema is a codegen
// error rather than a silent omission -- see `unsupported()`.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");
const SCHEMA_PATH = join(HERE, "project-v1.json");
const TS_OUT = join(ROOT, "sdks/vite-plugin/src/project-config/generated.ts");
const RS_OUT = join(ROOT, "crates/cli/src/project_config/generated.rs");

const schema = JSON.parse(readFileSync(SCHEMA_PATH, "utf8"));

function unsupported(where, detail) {
  throw new Error(
    `schema/codegen.mjs: unsupported construct at ${where}: ${detail}\n` +
      `Teach the generator, or express it with a construct it already knows. ` +
      `Silently skipping it would put a schema fact in NEITHER reader.`,
  );
}

/** Resolve one level of `$ref` into `$defs`, merging sibling keywords. */
function deref(node, where) {
  if (node == null || typeof node !== "object") unsupported(where, "not an object");
  if (!("$ref" in node)) return node;
  const ref = node.$ref;
  const m = /^#\/\$defs\/([A-Za-z0-9_]+)$/.exec(ref);
  if (!m) unsupported(where, `$ref "${ref}" is not #/$defs/<name>`);
  const target = schema.$defs?.[m[1]];
  if (!target) unsupported(where, `$defs/${m[1]} does not exist`);
  const { $ref: _drop, ...siblings } = node;
  return { ...target, ...siblings };
}

const KNOWN_KEYWORDS = new Set([
  "$schema", "$id", "$defs", "$ref", "title", "description", "type", "properties",
  "required", "additionalProperties", "items", "enum", "pattern", "default",
  "x-cli-read", "x-required-members", "x-forbidden-key-names", "x-config-filename",
  "x-config-env-var",
]);

function assertKnownKeywords(node, where) {
  for (const k of Object.keys(node)) {
    if (!KNOWN_KEYWORDS.has(k)) unsupported(where, `keyword "${k}"`);
  }
}

// ---------------------------------------------------------------------------
// Walk: collect every leaf with its dotted path, default, and markers.
// ---------------------------------------------------------------------------

/** @type {{path:string,type:string,default?:unknown,enum?:string[],pattern?:string,cliRead:boolean,itemPattern?:string}[]} */
const leaves = [];
/** @type {{path:string,known:string[],required:string[]}[]} */
const objects = [];

function recordLeaf(leaf) {
  const prior = leaves.find((candidate) => candidate.path === leaf.path);
  if (prior == null) {
    leaves.push(leaf);
    return;
  }
  const comparable = (value) => JSON.stringify({ ...value, cliRead: false });
  if (comparable(prior) !== comparable(leaf)) {
    unsupported(leaf.path, "root and environment definitions disagree");
  }
  prior.cliRead ||= leaf.cliRead;
}

function walkObject(node, path) {
  assertKnownKeywords(node, path || "<root>");
  if (node.type !== "object") unsupported(path, `expected type object, got ${node.type}`);
  if (node.additionalProperties !== false && node.additionalProperties !== undefined) {
    // The `environments` map uses additionalProperties as a SCHEMA (the env
    // entry shape). That is handled by the caller, not here.
    unsupported(path, "additionalProperties must be false on a closed object");
  }
  const props = node.properties ?? {};
  const known = Object.keys(props);
  const required = node["x-required-members"] ?? node.required ?? [];
  objects.push({ path, known, required });

  for (const [key, rawChild] of Object.entries(props)) {
    const childPath = path ? `${path}.${key}` : key;
    const child = deref(rawChild, childPath);
    assertKnownKeywords(child, childPath);
    const cliRead = child["x-cli-read"] === true;
    switch (child.type) {
      case "object":
        walkObject(child, childPath);
        break;
      case "array":
        if (child.items?.type !== "string") unsupported(childPath, "array items must be strings");
        recordLeaf({
          path: childPath, type: "string[]", default: child.default,
          cliRead, itemPattern: child.items.pattern,
        });
        break;
      case "string":
        recordLeaf({
          path: childPath, type: "string", default: child.default,
          enum: child.enum, pattern: child.pattern, cliRead,
        });
        break;
      case "boolean":
        recordLeaf({ path: childPath, type: "boolean", default: child.default, cliRead });
        break;
      default:
        unsupported(childPath, `type ${child.type}`);
    }
  }
}

// Root, minus `environments` (a map, walked separately because its members are
// a schema rather than a fixed property list).
const rootProps = { ...schema.properties };
const envNode = rootProps.environments;
delete rootProps.environments;
walkObject({ ...schema, properties: rootProps }, "");
// ...but it is still a KNOWN root key. Leaving it out here is how the first
// draft of this generator made every real file fail to parse with
// "unknown key `environments`": the walk skipped it and the known-key list is a
// by-product of the walk.
objects.find((o) => o.path === "").known.push("environments");

// The environment entry shape. Its members carry the SAME dotted paths as the
// root ones (an env `app` is the root `app` for that target), so the CLI-read
// set does not need a second spelling.
const envEntry = envNode.additionalProperties;
assertKnownKeywords(envEntry, "environments.*");
const envKnown = Object.keys(envEntry.properties);
const envRequired = envEntry.required ?? [];

function walkEnvironmentShape(rawNode, path) {
  const node = deref(rawNode, path);
  assertKnownKeywords(node, path);
  const cliRead = node["x-cli-read"] === true;
  switch (node.type) {
    case "object":
      for (const [key, child] of Object.entries(node.properties ?? {})) {
        walkEnvironmentShape(child, path ? `${path}.${key}` : key);
      }
      break;
    case "array":
      if (node.items?.type !== "string") unsupported(path, "array items must be strings");
      recordLeaf({
        path, type: "string[]", default: node.default, cliRead,
        itemPattern: node.items.pattern,
      });
      break;
    case "string":
      recordLeaf({
        path, type: "string", default: node.default, enum: node.enum,
        pattern: node.pattern, cliRead,
      });
      break;
    case "boolean":
      recordLeaf({ path, type: "boolean", default: node.default, cliRead });
      break;
    default:
      unsupported(path, `type ${node.type}`);
  }
}

for (const [key, child] of Object.entries(envEntry.properties)) {
  walkEnvironmentShape(child, key);
}

const TS_RESOLVED_INTERFACE_FIELDS = new Set([
  "name", "app", "control", "runtime_date", "build.mode", "build.serverEntry",
  "build.dist", "build.output", "migrations.dir", "migrations.out", "secrets",
  "protected",
]);
const RUST_VALIDATOR_FIELDS = new Set([
  "$schema", "name", "app", "control", "runtime_date", "build.mode",
  "build.serverEntry", "build.dist", "build.output", "migrations.dir",
  "migrations.out", "secrets", "protected",
]);

function assertReaderCoverage(reader, schemaFields, implementedFields) {
  const uncovered = [...schemaFields].filter((field) => !implementedFields.has(field));
  const stale = [...implementedFields].filter((field) => !schemaFields.has(field));
  if (uncovered.length || stale.length) {
    throw new Error(
      `schema/codegen.mjs: reader coverage gap in ${reader}: ` +
        `uncovered=[${uncovered.join(", ")}], stale=[${stale.join(", ")}]. ` +
        `Update the reader and its coverage declaration together.`,
    );
  }
}

const allLeafPaths = new Set(leaves.map((leaf) => leaf.path));
const resolvedLeafPaths = new Set(leaves.map((leaf) => leaf.path).filter((path) => path !== "$schema"));
assertReaderCoverage("TypeScript ResolvedProjectConfig", resolvedLeafPaths, TS_RESOLVED_INTERFACE_FIELDS);
assertReaderCoverage("Rust validator", allLeafPaths, RUST_VALIDATOR_FIELDS);

const cliReadFields = leaves.filter((l) => l.cliRead).map((l) => l.path);
const withDefaults = leaves.filter((l) => l.default !== undefined);
const requiredFields = new Set(
  objects.flatMap((o) => o.required.map((member) => o.path ? `${o.path}.${member}` : member)),
);
const rustResolvedDefaults = withDefaults.filter(
  (l) => !l.cliRead && !requiredFields.has(l.path),
);

// ---------------------------------------------------------------------------
// Emit
// ---------------------------------------------------------------------------

const BANNER_LINES = [
  "GENERATED by schema/codegen.mjs from schema/project-v1.json -- DO NOT EDIT.",
  "",
  "Regenerate with `node schema/codegen.mjs`; `tests/project_config_gate.sh`",
  "fails the build when this file drifts from the schema.",
];

function jsonLit(v) {
  return JSON.stringify(v);
}

function tsSource() {
  const L = [];
  L.push("/**");
  for (const line of BANNER_LINES) L.push(line ? ` * ${line}` : " *");
  L.push(" *");
  L.push(" * The plugin applies every default when there is no file. For a present file,");
  L.push(" * optional non-CLI defaults are also generated into Rust so the readers agree.");
  L.push(" * Defaults for CLI-read facts never reach Rust: silence there is an error.");
  L.push(" */");
  L.push("");
  L.push(`export const CONFIG_FILENAME = ${jsonLit(schema["x-config-filename"])};`);
  L.push(`export const CONFIG_ENV_VAR = ${jsonLit(schema["x-config-env-var"])};`);
  L.push(`export const SCHEMA_ID = ${jsonLit(schema.$id)};`);
  L.push("");
  L.push("/** Fields the Rust CLI also reads. The `config` escape hatch may not touch these. */");
  L.push(`export const CLI_READ_FIELDS: readonly string[] = ${jsonLit(cliReadFields)};`);
  L.push("");
  L.push("/** Key names that must never appear anywhere in the file. */");
  L.push(`export const FORBIDDEN_KEY_NAMES: readonly string[] = ${jsonLit(schema["x-forbidden-key-names"])};`);
  L.push("");
  for (const o of objects) {
    const id = o.path === "" ? "ROOT" : o.path.toUpperCase().replace(/\./g, "_");
    L.push(`export const ${id}_KNOWN_KEYS: readonly string[] = ${jsonLit(o.known)};`);
    L.push(`export const ${id}_REQUIRED_KEYS: readonly string[] = ${jsonLit(o.required)};`);
  }
  L.push(`export const ENVIRONMENT_KNOWN_KEYS: readonly string[] = ${jsonLit(envKnown)};`);
  L.push(`export const ENVIRONMENT_REQUIRED_KEYS: readonly string[] = ${jsonLit(envRequired)};`);
  L.push("");
  L.push("/** Every string field carrying a `pattern` or an `enum`, for validation. */");
  L.push("export const FIELD_RULES: readonly {");
  L.push("  path: string; type: \"string\" | \"boolean\" | \"string[]\";");
  L.push("  enum?: readonly string[]; pattern?: string; itemPattern?: string;");
  L.push(`}[] = ${jsonLit(leaves.map((l) => {
    const r = { path: l.path, type: l.type };
    if (l.enum) r.enum = l.enum;
    if (l.pattern) r.pattern = l.pattern;
    if (l.itemPattern) r.itemPattern = l.itemPattern;
    return r;
  }))};`);
  L.push("");
  L.push("/** Every schema `default`, by dotted path. The ONLY copy in the TS tree. */");
  L.push("export const DEFAULTS: Readonly<Record<string, unknown>> = {");
  for (const l of withDefaults) L.push(`  ${jsonLit(l.path)}: ${jsonLit(l.default)},`);
  L.push("};");
  L.push("");
  L.push("/** The shape a fully-resolved config takes. */");
  L.push("export interface ResolvedProjectConfig {");
  L.push("  name: string;");
  L.push("  app?: string;");
  L.push("  control: string;");
  L.push("  runtime_date: string;");
  L.push("  build: { mode: \"full\" | \"static\"; serverEntry?: string; dist: string; output: string };");
  L.push("  migrations: { dir: string; out: string };");
  L.push("  secrets: string[];");
  L.push("  protected?: boolean;");
  L.push("}");
  L.push("");
  return L.join("\n");
}

function rsSource() {
  const L = [];
  for (const line of BANNER_LINES) L.push(line ? `//! ${line}` : "//!");
  L.push("//!");
  L.push("//! THERE ARE NO DEFAULTS FOR CLI-READ FACTS IN THIS FILE.");
  L.push("//! A key the CLI operationally reads and the file omits is an error naming the");
  L.push("//! key. Optional non-CLI defaults are generated below from the same schema so");
  L.push("//! both readers still produce byte-identical resolved JSON.");
  L.push("");
  L.push(`pub const CONFIG_FILENAME: &str = ${jsonLit(schema["x-config-filename"])};`);
  L.push(`pub const CONFIG_ENV_VAR: &str = ${jsonLit(schema["x-config-env-var"])};`);
  L.push(`pub const SCHEMA_ID: &str = ${jsonLit(schema.$id)};`);
  L.push("");
  L.push("/// Fields the CLI reads. The Vite `config` escape hatch may not touch these.");
  L.push(`pub const CLI_READ_FIELDS: &[&str] = &[${cliReadFields.map(jsonLit).join(", ")}];`);
  L.push("");
  L.push("/// Key names that must never appear anywhere in the file.");
  L.push(`pub const FORBIDDEN_KEY_NAMES: &[&str] = &[${schema["x-forbidden-key-names"].map(jsonLit).join(", ")}];`);
  L.push("");
  for (const o of objects) {
    const id = o.path === "" ? "ROOT" : o.path.toUpperCase().replace(/\./g, "_");
    L.push(`pub const ${id}_KNOWN_KEYS: &[&str] = &[${o.known.map(jsonLit).join(", ")}];`);
    L.push(`pub const ${id}_REQUIRED_KEYS: &[&str] = &[${o.required.map(jsonLit).join(", ")}];`);
  }
  L.push(`pub const ENVIRONMENT_KNOWN_KEYS: &[&str] = &[${envKnown.map(jsonLit).join(", ")}];`);
  L.push(`pub const ENVIRONMENT_REQUIRED_KEYS: &[&str] = &[${envRequired.map(jsonLit).join(", ")}];`);
  L.push("");
  L.push("/// `(dotted path, regex-free validator tag)` for every constrained string.");
  L.push("///");
  L.push("/// The tag is matched in `super::validate`, which hand-writes each check: the");
  L.push("/// CLI has no regex crate and adding one for six patterns is not proportionate.");
  L.push("pub const FIELD_PATTERNS: &[(&str, &str)] = &[");
  for (const l of leaves) {
    if (l.pattern) L.push(`    (${jsonLit(l.path)}, ${jsonLit(l.pattern)}),`);
    if (l.itemPattern) L.push(`    (${jsonLit(l.path + "[]")}, ${jsonLit(l.itemPattern)}),`);
  }
  L.push("];");
  L.push("");
  L.push("/// `(dotted path, allowed values)` for every enum-constrained string.");
  L.push("pub const FIELD_ENUMS: &[(&str, &[&str])] = &[");
  for (const l of leaves) {
    if (l.enum) L.push(`    (${jsonLit(l.path)}, &[${l.enum.map(jsonLit).join(", ")}]),`);
  }
  L.push("];");
  L.push("");
  L.push("/// Every dotted path carrying a schema default.");
  L.push("///");
  L.push("/// Named rather than merely absent so the gate can assert the list is the exact");
  L.push("/// complete schema `default` set -- an omission here would read as \"no default");
  L.push("/// exists\" instead of a deliberate resolution rule.");
  L.push(`pub const SCHEMA_DEFAULTED_FIELDS: &[&str] = &[${withDefaults.map((l) => jsonLit(l.path)).join(", ")}];`);
  L.push("");
  L.push("/// Optional, non-CLI-read defaults applied to a present file's resolved view.");
  L.push("/// Values are JSON so arrays and future object defaults stay schema-generated.");
  L.push(`pub const RESOLVED_OPTIONAL_DEFAULTS_JSON: &[(&str, &str)] = &[${rustResolvedDefaults.map((l) => `(${jsonLit(l.path)}, ${jsonLit(JSON.stringify(l.default))})`).join(", ")}];`);
  L.push("");
  return L.join("\n");
}

const outputs = [
  [TS_OUT, tsSource()],
  [RS_OUT, rsSource()],
];

const check = process.argv.includes("--check");
let drifted = 0;
for (const [path, source] of outputs) {
  if (check) {
    let committed = "";
    try {
      committed = readFileSync(path, "utf8");
    } catch {
      console.error(`  MISSING ${path}`);
      drifted++;
      continue;
    }
    if (committed !== source) {
      console.error(`  DRIFT   ${path}`);
      drifted++;
    } else {
      console.log(`  ok      ${path}`);
    }
  } else {
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, source, "utf8");
    console.log(`  wrote   ${path}`);
  }
}

if (drifted > 0) {
  console.error(
    `schema/codegen.mjs --check: ${drifted} generated file(s) drifted from schema/project-v1.json. ` +
      `Run \`node schema/codegen.mjs\` and commit the result.`,
  );
  process.exit(1);
}
