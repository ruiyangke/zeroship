// The host-selected project schema needs no foreign-schema grant.
// Exercise guarded catalog probes through the real CLI and file-backed SQLite.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// The host suite builds and resolves its addon in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zeroship-migrate-node/zeroship-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_probe_scope";
const TABLE = "probe_scope_rows";

/** An optional foreign grant must not replace the host-selected project. */
function project(crossSchema: boolean): string {
  const work = mkdtempSync(join(HERE, "probescope-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1
${
  crossSchema
    ? `
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["analytics"] }
`
    : ""
}
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  // Adding an index exercises the catalog probe as well as table creation.
  writeFileSync(
    join(work, "migrations", "20260101000000_make.ts"),
    `import { table, t } from "@zeroship/migrate";
export const name = "make_rows";
export default {
  schema() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), val: t.int().notNull() },
      primaryKey: ["id"],
    });
    table("${TABLE}").index("${TABLE}_idx").add({ on: ["val"] });
  },
};
`,
  );
  return work;
}

function apply(work: string): Promise<{ code: number | null; text: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", `sqlite:${join(work, "app.db")}`,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim(),
      }),
    );
  });
}

for (const crossSchema of [false, true]) {
  test(`local probes apply and reapply with foreign grant ${crossSchema}`, async () => {
    const work = project(crossSchema);
    try {
      const applied = await apply(work);
      assert.equal(applied.code, 0, `local migration must apply: ${applied.text}`);
      const repeated = await apply(work);
      assert.equal(repeated.code, 0, `local migration must reapply: ${repeated.text}`);
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  });
}
