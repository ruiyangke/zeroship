// Index-element facets the renderer cannot carry are refused, not discarded.
//
// F652, the same shape as F651 and found by generalizing it: which options does
// the DSL ACCEPT that the engine then throws away? `dialects.md` names these two
// directly, in the list that also contained the F651 bug:
//
//   "Per-element `nulls` is unsupported."
//   "Expression elements cannot carry their own order, operator class,
//    collation, or null ordering."
//
// Both were accurate about the ENGINE and contradicted by the DSL, which accepted
// all five combinations and silently dropped them. Measured against live
// PostgreSQL before the fix:
//
//   { column: "e", nulls: "first" }     -> btree (e)                 nulls GONE
//   { expr: ..., order: "desc" }        -> btree (e)                 order GONE
//   { expr: ..., opclass: ... }         -> btree (e)                 opclass GONE
//   { expr: ..., collation: "C" }       -> btree (e)                 collation GONE
//   { expr: ..., nulls: "first" }       -> btree (e)                 nulls GONE
//
//   { column: "e", order: "desc" }      -> btree (e DESC)            CONTROL, works
//   { column: "e", opclass: ... }       -> btree (e text_pattern_ops) CONTROL, works
//   { column: "e", collation: "C" }     -> btree (e COLLATE "C")     CONTROL, works
//
// The controls are what make this a defect rather than "the renderer emits no
// modifiers": three of the same facets DO render on a column element, so the five
// omissions are genuine discards.
//
// WHY SILENCE IS THE DANGEROUS PART. The migration applies, the index exists, and
// only its ORDERING differs from what was authored. `{ expr, order: "desc" }`
// produced an ASCENDING index. Nothing fails, and the wrong plan shape is only
// visible to someone reading `pg_indexes` months later.
//
// THE TYPE SYSTEM CANNOT CATCH THE EXPRESSION CASES, which is why this file
// exists rather than a `@ts-expect-error` in `type-tests.ts`. `IndexElementArg` is
// a UNION, and TypeScript's excess-property check admits a property that exists on
// ANY member — `order`, `opclass` and `collation` all exist on
// `IndexColumnElementArg`. So `{ expr, order }` typechecks clean and must be
// refused at runtime. Only `nulls`, now absent from every member, is a type error.
//
// GATE: none. Every case is an offline authoring decision.

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

const OWNER_APP = "app_ixfacet";

/** `[label, element source]` — every facet the engine does not carry. */
const DISCARDED: ReadonlyArray<readonly [string, string]> = [
  ["column nulls", `{ column: "e", nulls: "first" }`],
  ["expr order", `{ expr: (col) => col("e"), order: "desc" }`],
  ["expr opclass", `{ expr: (col) => col("e"), opclass: "text_pattern_ops" }`],
  ["expr collation", `{ expr: (col) => col("e"), collation: "C" }`],
  ["expr nulls", `{ expr: (col) => col("e"), nulls: "first" }`],
];

/** `[label, element source]` — facets that genuinely render, and must keep doing so. */
const HONOURED: ReadonlyArray<readonly [string, string]> = [
  ["column order", `{ column: "e", order: "desc" }`],
  ["column opclass", `{ column: "e", opclass: "text_pattern_ops" }`],
  ["column collation", `{ column: "e", collation: "C" }`],
];

function project(element: string): string {
  const work = mkdtempSync(join(HERE, "ixfacet-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ ix_t: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  schema() {
    table("ix_t").create({
      columns: { id: t.int().notNull(), e: t.text() },
      primaryKey: ["id"],
    });
    table("ix_t").index("ix_a").add({ on: [${element}] });
  },
};
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

test("every facet the engine discards is refused instead", () => {
  for (const [what, element] of DISCARDED) {
    const work = project(element);
    try {
      const linted = lint(work);
      assert.equal(
        linted.code,
        1,
        `${what}: must FAIL CLOSED. Discarding it applies a migration whose index ` +
          `has different ordering than the author wrote, with no error at all; ` +
          `${linted.text}`,
      );
      assert.match(
        linted.text,
        /does not support/,
        `${what}: and the refusal must name the facet; ${linted.text}`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("CONTROL: the facets that DO render still lint clean", () => {
  // Without this, the refusals above are equally consistent with having banned
  // index-element facets outright -- which would be a worse bug than F652.
  for (const [what, element] of HONOURED) {
    const work = project(element);
    try {
      const linted = lint(work);
      assert.equal(
        linted.code,
        0,
        `${what}: renders on a column element and must keep working; ${linted.text}`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});
