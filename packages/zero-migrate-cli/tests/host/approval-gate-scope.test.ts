// WHICH operations the approval gate covers, measured against a live database.
//
// `--approve` is the operator decision that stands between a pipeline and data
// loss, and its scope is documented in two places:
//
//   writing-migrations.md  "Drops, unrestricted updates, deletes, and other
//                           destructive changes need careful review. Delete and
//                           backfill steps are always approval-gated."
//   cli.md                 "Deletes, backfills, online rename expansion, and
//                           other approval-gated work require an explicit
//                           operator decision"
//
// Nothing tested the scope as a whole. Individual tests cover individual gated
// operations, so a gate that had stopped covering one of them - or started
// covering something harmless and broken every pipeline - would show up as one
// failing test about that operation rather than as a change to the safety
// boundary.
//
// SO BOTH DIRECTIONS ARE PINNED. `addColumn` must apply WITHOUT approval; a gate
// that refused everything would satisfy every "is it gated" assertion and would
// be a different, equally serious defect.
//
// UPDATE IS DELIBERATELY RECORDED AS UNGATED, including the unrestricted form
// that rewrites every row. That matches the documentation - `update` appears in
// the "needs careful review" sentence and NOT in the "always approval-gated"
// one - and the distinction is easy to lose when skimming, which is exactly why
// it is written down here as a measured fact rather than left implicit. If the
// gate is ever widened to cover it, this file fails and says so.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);
const OWNER_APP = "app_approval_scope";

/** Every case, with whether the gate must stop it. */
const CASES: ReadonlyArray<readonly [string, string, boolean]> = [
  ["dropTable", `table("items").drop();`, true],
  ["dropColumn", `table("items").column("val").drop();`, true],
  ["delete", `table("items").delete({ where: (c) => c("id").eq(1) });`, true],
  [
    "backfill",
    `table("items").backfill({ set: { val: (c) => c("val").add(1) }, where: (c) => c("id").gt(0), cursorColumns: ["id"], cursorStability: { mode: "externalInvariant", name: "items_id" }, batchSize: 2, name: "bf" });`,
    true,
  ],
  // Documented as needing review, documented as NOT gated. Both forms.
  ["update (filtered)", `table("items").update({ set: { val: 9 }, where: (c) => c("id").eq(1) });`, false],
  ["update (all rows)", `table("items").update({ set: { val: 9 } });`, false],
  // The control: a plainly additive change must not need approval.
  ["addColumn", `table("items").column("extra").add({ type: t.text() });`, false],
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

const SEED = `table("items").create({
      columns: { id: t.int().notNull(), grp: t.text(), val: t.int() },
      primaryKey: ["id"],
    });
    table("items").insert({ rows: [{ id: 1, grp: "a", val: 1 }, { id: 2, grp: "b", val: 2 }] });`;

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "apprscope-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ items: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_seed.ts"),
    `import { table, t } from "zero-migrate";
export const name = "seed";
export default { up() { ${SEED} } };
`,
  );
  return work;
}

function run(work: string, schema: string, argv: string[]): { code: number | null; err: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", schema,
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return { code: result.status, err: (result.stderr ?? "").replace(/^WARNING.*$/gm, "").trim() };
}

test("the approval gate covers exactly the operations the documentation names", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const gatedButRan: string[] = [];
  const ungatedButRefused: string[] = [];
  try {
    for (const [label, body, mustBeGated] of CASES) {
      const schema = uniqueNamespace("apprscope");
      const work = project(schema);
      try {
        await client.query(`CREATE SCHEMA "${schema}"`);
        // The seed goes in WITH approval, so the measurement below is about the
        // operation under test rather than about the insert that set it up.
        assert.equal(
          run(work, schema, ["apply", "--approve"]).code,
          0,
          `${label}: the seed must apply`,
        );

        writeFileSync(
          join(work, "migrations", "20260102000000_op.ts"),
          `import { table, t } from "zero-migrate";
export const name = "op";
export default { up() { ${body} } };
`,
        );
        const unapproved = run(work, schema, ["apply"]);
        const refused = unapproved.code !== 0;

        if (mustBeGated && !refused) gatedButRan.push(label);
        if (!mustBeGated && refused) {
          ungatedButRefused.push(`${label}: ${unapproved.err.split("\n")[0]}`);
        }
        if (mustBeGated && refused) {
          assert.match(
            unapproved.err,
            /requires approval \(destructive\)/,
            `${label}: the refusal must say approval is what is missing`,
          );
        }
      } finally {
        await client
          .query(
            `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
             DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
          )
          .catch(() => {});
        rmSync(work, { recursive: true, force: true });
      }
    }

    assert.deepEqual(
      gatedButRan,
      [],
      "a destructive operation applied without approval - the gate has a hole",
    );
    // The other direction. A gate that refused everything would pass the
    // assertion above and break every pipeline that adds a column.
    assert.deepEqual(
      ungatedButRefused,
      [],
      "an operation the documentation does not gate was refused - the gate over-reaches",
    );
  } finally {
    await client.end().catch(() => {});
  }
});
