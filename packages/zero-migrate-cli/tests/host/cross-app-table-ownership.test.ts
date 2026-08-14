// One app may not migrate another app's table, measured against a live database.
//
// `README.md` lists "ownership and policy checks for platform-hosted migrations"
// as a headline capability, and it is the property that makes a shared schema safe
// for multiple tenants. Nothing asserted it. Searching both suites for the refusal
// text — "ownership violation", "may only migrate tables it owns", "deploying app
// is" — returns nothing; the "owned by" matches in other host tests are about
// INDEX NAME ownership, a different rule.
//
// The engine has an in-`src` unit test (`model/load.rs`,
// `version_gate_precedes_ownership_and_checksum`) but it asserts the ORDER in
// which gates fire on a fabricated artifact, not that a real deploy against a real
// server is stopped.
//
// THREE ARMS, two of them controls. A test that only proves a refusal cannot
// tell "ownership is enforced" from "this deploy was broken":
//
//   1. `app_a` creates its own table   - must succeed
//   2. `app_b` alters app_a's table    - must be REFUSED, and the message must
//      name the table, its owner, and the deploying app
//   3. `app_a` alters its own table    - must succeed, so arm 2 was about the
//      OWNER and not about that column, that op, or that directory
//
// DELIBERATELY NOT TESTED HERE: a second app deploying its OWN migrations into
// the same schema. Measured separately, that composes badly with the
// applied-prefix gate - the journal is per schema, so every deploy must supply
// every net-applied step, and two apps deploying from separate directories block
// each other after one round. Encoding it here would pin a configuration that
// only works once.
//
// Plus: the refused column must be ABSENT afterwards. A refusal that still landed
// the change would be the worst outcome of all, and the exit code alone does not
// rule it out.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
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

const APP_A = "app_a";
const APP_B = "app_b";

function migration(name: string, body: string): string {
  return `import { table, t } from "zero-migrate";
export const name = "${name}";
export default { schema() { ${body} } };
`;
}

function uniqueNamespace(): string {
  return `xapp_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/**
 * Each app deploys from its OWN directory. Sharing one directory would mean the
 * second app also re-reads the first app's migrations, and the refusal could come
 * from the wrong file — the first version of this test hit exactly that and
 * reported a stale success line as if it were the refusal.
 */
function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "xapp-"));
  for (const dir of ["a", "b", "b_intrudes"]) mkdirSync(join(work, dir));
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
`,
  );
  // BOTH tables registered, to different apps. The refusal must come from
  // ownership, not from a table being unknown to the registry.
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ alpha: APP_A, beta: APP_B }),
  );
  writeFileSync(
    join(work, "a", "20260101000000_a.ts"),
    migration("make_alpha", `table("alpha").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });`),
  );
  writeFileSync(
    join(work, "b", "20260102000000_b.ts"),
    migration("make_beta", `table("beta").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });`),
  );
  writeFileSync(
    join(work, "b_intrudes", "20260103000000_x.ts"),
    migration("touch_alpha", `table("alpha").column("sneaky").add({ type: t.int() });`),
  );
  return work;
}

function apply(work: string, dir: string, schema: string, app: string) {
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, dir),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", schema,
      "--owner-app", app,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  let out = "";
  let err = "";
  child.stdout.on("data", (chunk) => (out += chunk));
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; text: string }>((done) =>
    child.on("close", (code) =>
      done({ code, text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("an app cannot migrate a table another app owns", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace();
  const meta = `${schema}_migrations`;
  const work = project(schema);

  const columns = async (table: string): Promise<string[]> => {
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = $2 ORDER BY column_name`,
      [schema, table],
    );
    return rows.map((row) => row.column_name as string);
  };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    // ARM 1 — the owner creates its table.
    const madeAlpha = await apply(work, "a", schema, APP_A);
    assert.equal(madeAlpha.code, 0, `app_a must create its own table; ${madeAlpha.text}`);

    // ARM 2 — the intrusion.
    const intruded = await apply(work, "b_intrudes", schema, APP_B);
    assert.equal(intruded.code, 1, `app_b must be refused on app_a's table; ${intruded.text}`);
    // Asserted by CONTENT: a refusal for any other reason - a policy denial, a
    // missing grant, an unknown table - would also exit 1.
    assert.match(
      intruded.text,
      /ownership violation/,
      `the refusal must be the ownership gate: ${intruded.text}`,
    );
    for (const fragment of ["alpha", APP_A, APP_B]) {
      assert.match(
        intruded.text,
        new RegExp(fragment),
        `the refusal must name the table, its owner and the deploying app so an ` +
          `operator can act on it; missing ${fragment}: ${intruded.text}`,
      );
    }
    assert.ok(
      !(await columns("alpha")).includes("sneaky"),
      "the refused column must not exist - a refusal that still applied the change " +
        "would be worse than no gate at all",
    );

    // ARM 3 — the owner may do the very thing app_b was refused, so arm 2 was
    // about WHO, not about the column or the operation.
    writeFileSync(
      join(work, "a", "20260104000000_ok.ts"),
      migration("owner_adds", `table("alpha").column("fine").add({ type: t.int() });`),
    );
    const ownerAdds = await apply(work, "a", schema, APP_A);
    assert.equal(ownerAdds.code, 0, `the owner must be able to alter its table; ${ownerAdds.text}`);
    assert.ok(
      (await columns("alpha")).includes("fine"),
      "and the owner's column must actually land",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
