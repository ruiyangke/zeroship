// `lint` must reach the same verdict as `apply`, before touching a database.
//
// That is the whole point of the verb: catch a shape the target engine cannot
// run while it is still a file. The failure that matters is the FALSE GREEN - a
// migration that lints clean and then fails at deploy - because it defeats the
// only reason to run lint at all. The false alarm matters less but is not free:
// it makes authors distrust the verb and stop reading it.
//
// `index-facet-matrix.test.ts` pins the APPLY side of these same nine facets
// against real databases. This is the offline half, checked against the same
// declared support, so the two move together: if the engine's capability table
// changes and only one side is updated, one of these files fails.
//
// WHAT WAS ALREADY COVERED, and why it is not this. `cli.test.ts` has "lint
// defaults to all dialects and --dialect narrows", which exercises the plumbing
// on a migration that passes on all three. Every assertion there holds just as
// well for a lint whose capability check was wired to the wrong dialect, or
// skipped entirely - an all-green migration cannot tell those apart. Nothing
// exercised a verdict that has to come out DIFFERENT per dialect.
//
// The matrix is deliberately mixed rather than all-or-nothing. `expression` and
// `partial` are supported on SQLite and not on MySQL, so a lint that ignored the
// dialect and refused every facet everywhere would fail here on those two cells,
// and a lint that passed everything would fail on the rest. Uniformity in either
// direction is caught.
//
// GATE: none. `lint` is offline, so this runs everywhere.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

/** The nine index facets, as the source text that carries each one. */
const FACETS: ReadonlyArray<readonly [string, string]> = [
  ["expression", `{ on: [{ expr: (col) => col("rank").add(1) }] }`],
  ["partial", `{ on: [{ column: "email" }], where: (col) => col("rank").gt(0) }`],
  ["include", `{ on: [{ column: "email" }], include: ["rank"] }`],
  ["storage_params", `{ on: [{ column: "email" }], with: { fillfactor: 90 } }`],
  ["only", `{ on: [{ column: "email" }], only: true }`],
  ["nulls_not_distinct", `{ on: [{ column: "email" }], unique: true, nullsNotDistinct: true }`],
  ["opclass", `{ on: [{ column: "email", opclass: "text_pattern_ops" }] }`],
  ["collation", `{ on: [{ column: "email", collation: "C" }] }`],
  ["non_btree", `{ on: [{ column: "email" }], using: "gin" }`],
] as const;

/** Declared support, straight from `docs/support-matrix.md`. */
const SUPPORTED: Readonly<Record<string, ReadonlySet<string>>> = {
  postgres: new Set(FACETS.map(([name]) => name)),
  mysql: new Set<string>(),
  sqlite: new Set(["expression", "partial"]),
};

const DIALECTS = ["postgres", "mysql", "sqlite"] as const;

function project(facetOptions: string): string {
  const work = mkdtempSync(join(HERE, "lintverdict-"));
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
`,
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_m.ts"),
    `import { table, t } from "zero-migrate";
export const name = "indexed";
export default {
  schema() {
    table("users").create({
      columns: { id: t.int().notNull(), email: t.text(), rank: t.int() },
      primaryKey: ["id"],
    });
    table("users").index("ix_users").add(${facetOptions});
  },
};
`,
  );
  return work;
}

function lint(work: string, dialect: string): { status: number | null; output: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "lint",
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--owner-app", "app_lint_verdicts",
      "--dialect", dialect,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    status: result.status,
    output: `${result.stdout ?? ""}${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("lint's per-dialect verdict matches declared support for every index facet", () => {
  const falseGreens: string[] = [];
  const falseAlarms: string[] = [];
  let supportedSeen = 0;
  let unsupportedSeen = 0;

  for (const [name, options] of FACETS) {
    const work = project(options);
    try {
      for (const dialect of DIALECTS) {
        const declared = SUPPORTED[dialect].has(name);
        const { status, output } = lint(work, dialect);
        const passed = status === 0;
        if (declared) supportedSeen += 1;
        else unsupportedSeen += 1;

        if (declared && !passed) {
          falseAlarms.push(`${name}/${dialect}: ${output.split("\n")[0]}`);
        }
        if (!declared && passed) {
          falseGreens.push(`${name}/${dialect}`);
        }
      }
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }

  // Both counts are asserted so the loop cannot silently stop covering one side.
  // An all-refusing lint would leave `supportedSeen` cells failing; an
  // all-passing one would leave `unsupportedSeen` cells failing. Neither can
  // masquerade as agreement.
  assert.equal(supportedSeen, 11, "eleven facet/dialect cells are declared supported");
  assert.equal(unsupportedSeen, 16, "sixteen are declared unsupported");

  assert.deepEqual(
    falseGreens,
    [],
    "lint passed a shape its dialect cannot run - this is the failure that defeats the verb",
  );
  assert.deepEqual(
    falseAlarms,
    [],
    "lint refused a shape its dialect supports",
  );
});

test("a refusal names the dialect it is about, so an author knows which target to fix", () => {
  // `non_btree` is supported on PostgreSQL and not on the other two, so the same
  // file produces a different verdict per dialect. Without naming the dialect, a
  // default `lint` over all three would leave the author guessing which one.
  const work = project(`{ on: [{ column: "email" }], using: "gin" }`);
  try {
    for (const dialect of ["mysql", "sqlite"] as const) {
      const { status, output } = lint(work, dialect);
      assert.equal(status, 1, `${dialect} must refuse a non-btree index method`);
      assert.match(
        output,
        new RegExp(`\\b${dialect}\\b`),
        `the refusal must name ${dialect}; got: ${output}`,
      );
    }
    const pg = lint(work, "postgres");
    assert.equal(pg.status, 0, `PostgreSQL supports it; got: ${pg.output}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("an index method the IR does not define is refused with the defined ones named", () => {
  // `hash` and `spgist` are real PostgreSQL methods that this IR does not model.
  // Refusing is right; the refusal has to say what IS available, or the author
  // has no way to know the list without reading the crate.
  for (const method of ["hash", "spgist"]) {
    const work = project(`{ on: [{ column: "email" }], using: ${JSON.stringify(method)} }`);
    try {
      const { status, output } = lint(work, "postgres");
      assert.equal(status, 1, `${method} is not a modelled index method`);
      assert.match(
        output,
        new RegExp(`unknown variant \`${method}\``),
        `the refusal must name the rejected method; got: ${output}`,
      );
      assert.match(
        output,
        /btree.*brin.*gin.*gist/s,
        `the refusal must list the methods that ARE defined; got: ${output}`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

/** The vector/geoPoint derived index is a different shape from the facets above -
 *  it is declared by the COLUMN TYPE rather than by an index option - so it gets
 *  its own arm rather than a row in the matrix.
 *
 *  It belongs in this file because it is where lint and apply most recently
 *  disagreed. `lint --dialect mysql` passed these declarations while apply refused
 *  three of the four, since MySQL cannot build an index over a `blob` and cannot
 *  build a SPATIAL one over a nullable column. Apply no longer emits that index on
 *  MySQL, so lint's green is now the CORRECT verdict on every target - and this
 *  arm is what fails if someone later teaches lint to refuse these types without
 *  teaching apply the same thing.
 *
 *  The live half is `ann-index-only-where-buildable.test.ts`, which asserts the
 *  apply side against real databases and that MySQL really carries no index. */
test("vector and geoPoint lint clean on every dialect, matching what apply now does", () => {
  const COLUMNS: ReadonlyArray<readonly [string, string]> = [
    ["vector", `t.vector({ dimensions: 3, metric: "cosine" })`],
    ["geoPoint", `t.geoPoint()`],
    ["geoPoint notNull", `t.geoPoint().notNull()`],
  ];
  for (const [what, column] of COLUMNS) {
    const work = mkdtempSync(join(HERE, "lintann-"));
    try {
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
`,
      );
      writeFileSync(
        join(work, "migrations", "20260101000000_m.ts"),
        `import { table, t } from "zero-migrate";
export const name = "typed";
export default {
  schema() {
    table("things").create({
      columns: { id: t.int().notNull(), payload: ${column} },
      primaryKey: ["id"],
    });
  },
};
`,
      );
      for (const dialect of DIALECTS) {
        const { status, output } = lint(work, dialect);
        assert.equal(
          status,
          0,
          `${what} must lint clean on ${dialect}, since apply accepts it there; got: ${output}`,
        );
      }
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});
