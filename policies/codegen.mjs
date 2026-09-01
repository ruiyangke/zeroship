#!/usr/bin/env node
//
// Emit the TypeScript view of policies/confined-system-shape.inject.toml.
//
// WHY THIS EXISTS. The confined [[inject]] rule is the shape of every creator
// table on the platform, and it used to be written out by hand in six places.
// Rust consumers now take the fragment directly -
// `concat!(include_str!("<grants>"), include_str!("<fragment>"))` - so for them
// there is nothing to generate and nothing that can drift. TypeScript has no
// include_str!, and `sdks/vite-plugin` is a PUBLISHED package that runs on a
// creator's machine where this repo does not exist, so it cannot read the file
// at runtime either. The bytes have to be carried into a module, and something
// has to put them there.
//
// WHY THE OUTPUT IS COMMITTED rather than gitignored and built. Generating into
// an untracked file would make `tsc`, `tsx` and every editor depend on this
// script having run first. That failure IS loud - measured 2026-08-20, absent
// output gives `error TS2307: Cannot find module './confined-system-shape.
// generated.js'` (tsc exit 2) and `ERR_MODULE_NOT_FOUND` under tsx - but loud is
// not the same as informative: neither message names the step you were supposed
// to run. AGENTS.md spends twenty lines on exactly that shape of error, because
// a loud TS2307 from the zeroship-migrate-node addon still cost a real deploy twenty
// minutes. Committing the output keeps the build order flat and moves the whole
// question onto a drift check, which is what `schema/codegen.mjs` (the sibling
// generator, project-v1.json -> project-config/generated.ts) already does.
//
// THE DRIFT CHECK is `--check`, run by tests/inject_policy_mirror_gate.sh. It
// regenerates in memory and compares BYTE FOR BYTE, so unlike the text-mirroring
// gate this replaces it also catches a comment or a blank line going stale.
//
// TWO VIEWS ARE EMITTED, not one, because the two TypeScript consumers ask
// different questions of the same bytes. sdks/vite-plugin loads a policy
// DOCUMENT and needs the TOML verbatim; sdks/db makes a per-field decision at
// insert time and needs the rule already projected into data, because shipping
// it the TOML would mean shipping a TOML parser into the data plane. Both are
// committed, both are byte-compared by --check.
//
// Usage:
//   node policies/codegen.mjs           write the generated files
//   node policies/codegen.mjs --check   exit 1 if either differs from the fragment

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");

const FRAGMENT = join(ROOT, "policies", "confined-system-shape.inject.toml");

// TWO targets, because the two TypeScript consumers need different things and
// only one of them can use raw bytes.
//
//   sdks/vite-plugin  loads a POLICY DOCUMENT. It concatenates its own grants
//                     onto the fragment and hands the result to the migration
//                     engine, so it needs the TOML verbatim.
//   sdks/db           makes a PER-FIELD decision at insert time ("does the
//                     platform compute this value?"). It has no TOML parser and
//                     must not grow one, so it needs the fragment already
//                     projected into data.
//
// Same authored bytes, two views, both regenerated and byte-compared by
// tests/inject_policy_mirror_gate.sh.
const TARGET_TOML = join(
  ROOT,
  "sdks",
  "vite-plugin",
  "src",
  "gen-types",
  "confined-system-shape.generated.ts",
);
const TARGET_PROJECTION = join(
  ROOT,
  "sdks",
  "db",
  "src",
  "generated",
  "confined-system-shape.generated.ts",
);

const fragment = readFileSync(FRAGMENT, "utf8");

// The fragment is embedded as a TEMPLATE LITERAL so the generated file stays
// readable - a committed file gets read, and JSON.stringify would turn the rule
// into one unreadable line. That only works while the fragment contains no
// backtick, no `${` and no backslash, which is also true of the .toml it has to
// remain. Refuse rather than emit something that compiles to different bytes.
for (const [needle, what] of [
  ["`", "a backtick"],
  ["${", "a template-literal substitution"],
  ["\\", "a backslash"],
]) {
  if (fragment.includes(needle)) {
    console.error(
      `policies/codegen.mjs: the fragment contains ${what}, which does not survive\n` +
        "  embedding as a template literal. Reword the comment (the TOML rule itself\n" +
        "  needs none of these characters), or change this script to emit an escaped\n" +
        "  string and accept that the generated file becomes unreadable.",
    );
    process.exit(2);
  }
}

// Anti-vacuity: an empty or truncated read would generate a ceiling that injects
// nothing, and every consumer would still compile.
if (!/^\[\[inject\]\]$/m.test(fragment) || !/^author_primary_key\s*=/m.test(fragment)) {
  console.error(
    `policies/codegen.mjs: ${relative(ROOT, FRAGMENT)} carries no [[inject]] rule.\n` +
      "  Generating from it would emit a ceiling that injects nothing, which every\n" +
      "  consumer would accept in silence.",
  );
  process.exit(2);
}

// ---------------------------------------------------------------------------
// The projection.
//
// WHY PARSE AT ALL, when the sibling target ships the bytes untouched. Because
// `@zeroship/db` runs in the worker and in every creator's `pnpm dev`, and the
// question it asks of this file - "is this column one the platform computes?" -
// is answered per field, per insert. Shipping it the TOML would mean shipping a
// TOML parser into the data plane and running it at boot to answer a question
// whose answer is fixed at commit time. Parse once, here, and commit the answer.
//
// WHAT IS DELIBERATELY NOT PROJECTED: `default`. The two are layers, not
// duplicates - the generator is the normal path and the DDL default is the
// backstop for writes that never reach the runtime - and `deleted_at` is the
// proof that they must not be conflated: it is TIMESTAMPTZ NULL with NO default
// and `assign.by = "now"`. A projection that rendered a default from `by` would
// make every row born soft-deleted. The SDK needs `assign` and nothing else, so
// it gets `assign` and nothing else.
//
// This is a strict parser over a file whose column lines are one inline table
// each. It refuses rather than guesses: a line inside `columns = [ ... ]` that
// it cannot read is an error, not a skip. A lenient parser here would silently
// project fewer columns, and every consumer would accept the shorter list.
function parseInjectColumns(text) {
  const lines = text.split("\n");
  const start = lines.findIndex((l) => /^columns\s*=\s*\[\s*$/.test(l));
  if (start === -1) {
    console.error(
      "policies/codegen.mjs: no `columns = [` line in the fragment. The rule's\n" +
        "  column list moved or changed shape; the projection cannot be generated.",
    );
    process.exit(2);
  }
  const end = lines.findIndex((l, i) => i > start && /^\]\s*$/.test(l));
  if (end === -1) {
    console.error(
      "policies/codegen.mjs: the `columns = [` list is not closed by a `]` at the\n" +
        "  start of a line. Refusing to guess where it ends.",
    );
    process.exit(2);
  }

  const columns = [];
  for (let i = start + 1; i < end; i++) {
    const line = lines[i];
    if (/^\s*$/.test(line) || /^\s*#/.test(line)) continue;

    const body = /^\s*\{(.*)\}\s*,?\s*$/.exec(line);
    if (body === null) {
      console.error(
        `policies/codegen.mjs: line ${i + 1} of the fragment is inside\n` +
          "  `columns = [ ... ]` but is neither a comment nor a `{ ... }` inline table:\n" +
          `    ${line}\n` +
          "  Refusing to emit a projection that silently omits it.",
      );
      process.exit(2);
    }
    const inner = body[1];

    const name = /(?:^|[,{]|\s)name\s*=\s*"([^"]*)"/.exec(inner);
    if (name === null) {
      console.error(
        `policies/codegen.mjs: the column on line ${i + 1} has no \`name\`:\n    ${line}`,
      );
      process.exit(2);
    }
    const nullable = /(?:^|[,{]|\s)nullable\s*=\s*(true|false)/.exec(inner);
    if (nullable === null) {
      console.error(
        `policies/codegen.mjs: the column on line ${i + 1} has no \`nullable\`:\n    ${line}\n` +
          "  Nullability is projected, so an absent one cannot be defaulted here.",
      );
      process.exit(2);
    }

    // `assign = { by = "...", on = "..." }`. Optional: a charter column without
    // one is a column the caller supplies.
    const assign = /(?:^|[,{]|\s)assign\s*=\s*\{([^}]*)\}/.exec(inner);
    let projected = { name: name[1], nullable: nullable[1] === "true" };
    if (assign !== null) {
      const by = /(?:^|[,{]|\s)by\s*=\s*"([^"]*)"/.exec(assign[1]);
      const on = /(?:^|[,{]|\s)on\s*=\s*"([^"]*)"/.exec(assign[1]);
      if (by === null || on === null) {
        console.error(
          `policies/codegen.mjs: the \`assign\` on line ${i + 1} is missing \`by\` or \`on\`:\n` +
            `    ${line}\n` +
            "  Both halves are load-bearing: `by` names the generator, `on` names when\n" +
            "  it runs. Half an assignment is not a projectable one.",
        );
        process.exit(2);
      }
      if (!["insert", "write", "delete"].includes(on[1])) {
        console.error(
          `policies/codegen.mjs: line ${i + 1} assigns on "${on[1]}", which is not one of\n` +
            "  insert / write / delete. `on` is a closed vocabulary; widening it is a\n" +
            "  design change that has to reach every consumer, not a new string.",
        );
        process.exit(2);
      }
      projected = { ...projected, assign: { by: by[1], on: on[1] } };
    }
    columns.push(projected);
  }

  // Anti-vacuity, the same shape as the `[[inject]]` guard above: a column list
  // that parsed to nothing would generate a projection under which NO field is
  // platform-assigned, and every consumer would accept it in silence - the SDK
  // would go back to demanding `id` from the caller, which is the exact defect
  // this projection exists to remove.
  if (columns.length === 0) {
    console.error(
      "policies/codegen.mjs: the fragment's `columns` list parsed to zero columns.\n" +
        "  Generating from it would emit a projection in which nothing is platform-\n" +
        "  assigned, and every consumer would accept that in silence.",
    );
    process.exit(2);
  }
  if (!columns.some((c) => c.assign !== undefined)) {
    console.error(
      "policies/codegen.mjs: no column in the fragment carries an `assign`.\n" +
        "  The projection exists to carry assignment bindings; one with none is\n" +
        "  indistinguishable from a parser that stopped matching them.",
    );
    process.exit(2);
  }

  return columns;
}

const columns = parseInjectColumns(fragment);

const renderColumn = (c) => {
  const assign =
    c.assign === undefined
      ? ""
      : `, assign: { by: ${JSON.stringify(c.assign.by)}, on: ${JSON.stringify(c.assign.on)} }`;
  return `  { name: ${JSON.stringify(c.name)}, nullable: ${c.nullable}${assign} },`;
};

const projectionSource = `/**
 * GENERATED by policies/codegen.mjs from
 * policies/confined-system-shape.inject.toml -- DO NOT EDIT.
 *
 * Regenerate with \`node policies/codegen.mjs\`;
 * \`tests/inject_policy_mirror_gate.sh\` fails when this file drifts from the
 * fragment.
 *
 * The PROJECTED view of the platform system-table shape, for \`@zeroship/db\`.
 * The sibling view (sdks/vite-plugin/src/gen-types/confined-system-shape.
 * generated.ts) carries the same rule as raw TOML because its consumer loads a
 * policy document; this one carries it as data because its consumer asks a
 * per-field question at insert time and has no TOML parser.
 *
 * WHAT THIS IS FOR. Each column here declares who computes its value and when.
 * A column carrying an \`assign\` is computed by the platform, from which two
 * things follow and neither is stored separately: the caller is not required to
 * supply it, and no value may be materialised for it on the way to the native
 * op. A column with no \`assign\` is an ordinary one the caller owns.
 *
 * Read the fragment, not this file, for what the rule means and why it is
 * shared.
 */

/** When a generator runs. A closed vocabulary; see the fragment. */
export type PlatformAssignmentEvent = "insert" | "write" | "delete";

/**
 * Who computes a column's value, and when.
 *
 * \`by\` names a generator (\`now\`, \`typedId\`, \`actor\`, \`increment(1)\`,
 * \`identity\`) and may carry charter-level CONSTANT arguments only - never
 * per-collection creator data. The typed-id prefix, for instance, is not here:
 * it is per-collection input resolved at generation time.
 */
export interface PlatformAssignment {
  readonly by: string;
  readonly on: PlatformAssignmentEvent;
}

/** One column of the confined [[inject]] rule. */
export interface PlatformColumn {
  readonly name: string;
  readonly nullable: boolean;
  /** Present iff the platform computes this column's value. */
  readonly assign?: PlatformAssignment;
}

/**
 * Every column the charter injects into every creator table, in fragment order.
 */
export const CONFINED_SYSTEM_SHAPE_COLUMNS: readonly PlatformColumn[] = Object.freeze([
${columns.map(renderColumn).join("\n")}
]);

/**
 * The injected column names. Derived from {@link CONFINED_SYSTEM_SHAPE_COLUMNS}
 * rather than emitted as a second literal, so the two cannot disagree.
 */
export const CONFINED_SYSTEM_SHAPE_COLUMN_NAMES: readonly string[] = Object.freeze(
  CONFINED_SYSTEM_SHAPE_COLUMNS.map((c) => c.name),
);

/**
 * Column name -> assignment, for the columns that carry one. Also derived.
 *
 * A lookup miss means "the caller owns this column", which is the right answer
 * for every creator-declared field as well as for any charter column that
 * declares no generator.
 */
export const CONFINED_SYSTEM_SHAPE_ASSIGNMENTS: Readonly<
  Record<string, PlatformAssignment>
> = Object.freeze(
  Object.fromEntries(
    CONFINED_SYSTEM_SHAPE_COLUMNS.flatMap((c) =>
      c.assign === undefined ? [] : [[c.name, c.assign] as const],
    ),
  ),
);
`;

const tomlSource = `/**
 * GENERATED by policies/codegen.mjs from
 * policies/confined-system-shape.inject.toml -- DO NOT EDIT.
 *
 * Regenerate with \`node policies/codegen.mjs\`;
 * \`tests/inject_policy_mirror_gate.sh\` fails when this file drifts from the
 * fragment.
 *
 * This is the TypeScript view of the ONE copy of the platform system-table
 * shape. The Rust consumers (the deployed server ceiling, the dev SQLite
 * ceiling, and the two adapter fixtures) \`include_str!\` the fragment itself and
 * need no generated form. Read the fragment, not this file, for what the rule
 * means and why it is shared.
 */

export const CONFINED_SYSTEM_SHAPE_INJECT_TOML = \`${fragment}\`;
`;

const TARGETS = [
  { path: TARGET_TOML, source: tomlSource },
  { path: TARGET_PROJECTION, source: projectionSource },
];

const check = process.argv.includes("--check");

if (!check) {
  for (const target of TARGETS) {
    mkdirSync(dirname(target.path), { recursive: true });
    writeFileSync(target.path, target.source, "utf8");
    console.log(`  wrote   ${relative(ROOT, target.path)}`);
  }
  process.exit(0);
}

// Every target is checked before exiting, so one run names every stale file
// rather than only the first. A partial regeneration - one view committed, the
// other not - is exactly the state this reports best.
let drifted = 0;
let refused = 0;
for (const target of TARGETS) {
  const rel = relative(ROOT, target.path);
  let committed;
  try {
    committed = readFileSync(target.path, "utf8");
  } catch {
    console.error(`  MISSING ${rel}`);
    refused++;
    continue;
  }
  if (committed !== target.source) {
    console.error(`  DRIFT   ${rel}`);
    drifted++;
    continue;
  }
  console.log(`  ok      ${rel}`);
}

if (refused > 0) {
  console.error(
    "policies/codegen.mjs --check: a generated file is absent. Run " +
      "`node policies/codegen.mjs` and commit the result.",
  );
  process.exit(1);
}
if (drifted > 0) {
  console.error(
    "policies/codegen.mjs --check: a generated file drifted from " +
      "policies/confined-system-shape.inject.toml. Run `node policies/codegen.mjs` " +
      "and commit the result.",
  );
  process.exit(1);
}
