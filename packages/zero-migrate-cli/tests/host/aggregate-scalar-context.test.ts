// An aggregate is refused in every scalar position the docs list, by name.
//
// `writing-migrations.md`: "Aggregates belong in view projections or `having`, not
// in defaults, checks, generated columns, index expressions, assignments, or
// ordinary predicates."
//
// That is six forbidden positions in one sentence, which makes it the kind of claim
// worth measuring rather than reading: five holding and one not would look exactly
// like six holding from any single example. `countStar` had no host coverage at all.
//
// All six refuse, each with `AGGREGATE_IN_SCALAR_CONTEXT` and a message naming the
// position it came from ("the column \"n\".default", "the CHECK constraint", "the
// generated column expression", "the index expression", "the update assignment to
// \"n\"", "the update where predicate").
//
// THE ASSERTION IS THE ERROR CODE, NOT THE EXIT CODE, and that is not fussiness.
// While writing this I passed `.generated({ as: …, stored: true })`, which is not
// the API — the real shape is a positional callback. That case exited 1 with
// "generated column expression must be a (col) => Expr callback", so on exit status
// alone it read as a passing sixth case while the aggregate rule in that position
// went completely untested. Asserting the code is what caught it.
//
// The two controls exist for the same reason:
//
//   a VALID generated column must lint CLEAN — proving the call shape is right, so
//   a refusal in that position is about the aggregate rather than the syntax;
//   a view projection must ACCEPT the same aggregate — proving the rule is
//   positional rather than a blanket ban on `countStar`, which would satisfy all
//   six refusals just as well.
//
// GATE: none. Every case is an offline lint decision.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_aggscalar";

/** The six positions the documentation forbids, in its own order. */
const FORBIDDEN: ReadonlyArray<readonly [string, string, string]> = [
  [
    "default",
    `import { table, t, countStar } from "zero-migrate";`,
    `table("agg_x").create({ columns: { id: t.int().notNull(), n: t.int().default(countStar()) }, primaryKey: ["id"] });`,
  ],
  [
    "check",
    `import { table, countStar } from "zero-migrate";`,
    `table("agg_t").check("agg_ck").add({ expr: () => countStar().gt(0) });`,
  ],
  [
    "generated column",
    `import { table, t, countStar } from "zero-migrate";`,
    `table("agg_y").create({ columns: { id: t.int().notNull(), g: t.int().generated(() => countStar()) }, primaryKey: ["id"] });`,
  ],
  [
    "index expression",
    `import { table, countStar } from "zero-migrate";`,
    `table("agg_t").index("agg_ix").add({ on: [{ expr: () => countStar() }] });`,
  ],
  [
    "assignment",
    `import { table, countStar } from "zero-migrate";`,
    `table("agg_t").update({ set: { n: () => countStar() }, where: (col) => col("id").gt(0) });`,
  ],
  [
    "where predicate",
    `import { table, countStar } from "zero-migrate";`,
    `table("agg_t").update({ set: { n: () => 1 }, where: () => countStar().gt(0) });`,
  ],
];

function project(imports: string, body: string): string {
  const work = mkdtempSync(join(HERE, "aggscalar-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ agg_t: OWNER_APP, agg_x: OWNER_APP, agg_y: OWNER_APP, agg_v: OWNER_APP }),
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("agg_t").create({
      columns: { id: t.int().notNull(), n: t.int() },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `${imports}
export const name = "b";
export default { up() { ${body} } };
`,
  );
  return work;
}

function lint(work: string): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "lint", "--dialect", "postgres",
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("an aggregate is refused in all six documented scalar positions", () => {
  for (const [what, imports, body] of FORBIDDEN) {
    const work = project(imports, body);
    try {
      const linted = lint(work);
      assert.equal(linted.code, 1, `${what}: must be refused; ${linted.text}`);
      // BY CODE, not by exit status: a malformed call also exits 1, and that is
      // exactly how a wrong `.generated(...)` shape once masqueraded as a passing
      // case here while testing nothing.
      assert.match(
        linted.text,
        /AGGREGATE_IN_SCALAR_CONTEXT/,
        `${what}: must be refused BY THE AGGREGATE RULE, not for some other ` +
          `reason; got: ${linted.text}`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("CONTROL: the same generated-column shape without an aggregate lints clean", () => {
  // Proves the refusal above is about the aggregate and not about the syntax.
  const work = project(
    `import { table, t } from "zero-migrate";`,
    `table("agg_y").create({ columns: { id: t.int().notNull(), a: t.int().notNull(), g: t.int().generated((col) => col("a").add(1)) }, primaryKey: ["id"] });`,
  );
  try {
    const linted = lint(work);
    assert.equal(
      linted.code,
      0,
      `a valid generated column must lint clean, or the aggregate refusal in that ` +
        `position proves nothing; ${linted.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("CONTROL: the same aggregate is accepted in a view projection", () => {
  // Proves the rule is POSITIONAL. A blanket ban on countStar would satisfy every
  // refusal above while breaking the one place aggregates belong.
  const work = project(
    `import { view, countStar } from "zero-migrate";`,
    `view("agg_v").create({ as: (q) => q.from("agg_t").select([{ kind: "expr", alias: "total", expr: () => countStar() }]) });`,
  );
  try {
    const linted = lint(work);
    assert.equal(
      linted.code,
      0,
      `an aggregate must still be allowed where the docs say it belongs; ${linted.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
