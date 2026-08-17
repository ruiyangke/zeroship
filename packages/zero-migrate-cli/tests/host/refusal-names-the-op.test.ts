// A refusal must show WHICH op it is about, not just an ordinal.
//
// The engine stamps every `AuthoringError` with `op_index`, and the CLI printed
// that number verbatim:
//
//   mysql: ... [UNSUPPORTED kind=op op_index=1 dialect=mysql]: createIndex
//          BRIN/INCLUDE/WITH/ONLY features are unsupported on MySQL
//
// `op_index=1` is only useful to someone holding the rendered IR envelope. An
// author holds a `.ts` FILE, and the mapping from source statements to IR ops is
// not one-to-one - a single `table(...).create({...})` fans out into several ops,
// and the recorder is free to reorder or synthesise. So the operator's actual
// question, "which of my statements is this about", had no answer in the output.
//
// The envelope IS in scope at the render site: `runLint` already reads
// `envelope.ops.length` to print the op count on the very same line. Indexing
// that same array by the `op_index` the refusal already carries costs nothing
// and turns an ordinal into the offending op itself.
//
// WHY THE NEGATIVE ARM IS HERE TOO. "Prints an op" also holds for a build that
// prints the FIRST op every time, or every op, which would be worse than the
// ordinal because it reads as precise while being wrong. The migration below is
// therefore built so the refused op is NOT op 0: `table("users").create(...)`
// occupies index 0 and lints clean on every dialect, and the `using: "gin"`
// index is what MySQL refuses. The test asserts the printed op is the
// createIndex AND that the createTable is absent from the refusal.
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

/** A two-op migration whose SECOND op is the one MySQL refuses. */
function project(): string {
  const work = mkdtempSync(join(HERE, "refusalop-"));
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
    table("users").index("ix_users").add({ on: [{ column: "email" }], using: "gin" });
  },
};
`,
  );
  return work;
}

function lint(work: string, extra: readonly string[] = []): { status: number | null; output: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "lint",
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--owner-app", "app_refusal_names_op",
      "--dialect", "mysql",
      ...extra,
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

test("a refusal prints the offending op, indexed out of the envelope by op_index", () => {
  const work = project();
  try {
    const { status, output } = lint(work);
    assert.equal(status, 1, `MySQL must refuse a gin index; got: ${output}`);

    // The precondition this whole feature rests on: the engine really does stamp
    // a non-zero op_index here. If the engine stops carrying it, the assertions
    // below would pass vacuously against an empty rendering, so pin it.
    assert.match(
      output,
      /op_index=1\b/,
      `the refusal must still carry op_index=1; got: ${output}`,
    );

    // The op itself, not the ordinal.
    assert.match(
      output,
      /"op"\s*:\s*"createIndex"/,
      `the refusal must print the offending op's JSON; got: ${output}`,
    );
    assert.match(
      output,
      /ix_users/,
      `the printed op must be the one that was refused; got: ${output}`,
    );

    // The negative arm: printing op 0, or every op, would satisfy the two
    // assertions above while telling the author the wrong thing.
    assert.doesNotMatch(
      output,
      /"op"\s*:\s*"createTable"/,
      `only the offending op may be printed, not the whole envelope; got: ${output}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("--json carries the offending op as structured data, not only in the human text", () => {
  // A gate parsing `lint --json` is exactly the consumer that cannot re-derive
  // the op from a prose line, so the machine channel has to carry it too.
  const work = project();
  try {
    const { status, output } = lint(work, ["--json"]);
    assert.equal(status, 1, `MySQL must refuse a gin index; got: ${output}`);
    const reports = JSON.parse(output) as Array<{
      dialects: Array<{ ok: boolean; error?: string; op?: unknown }>;
    }>;
    const failed = reports.flatMap((r) => r.dialects).filter((d) => !d.ok);
    assert.equal(failed.length, 1, `exactly one dialect verdict must fail; got: ${output}`);
    assert.deepEqual(
      failed[0].op,
      {
        op: "createIndex",
        table: "users",
        columns: [{ kind: "column", name: "email" }],
        name: "ix_users",
        using: "gin",
      },
      `the failing verdict must carry the offending op verbatim; got: ${output}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
