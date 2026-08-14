// `ZERO_MIGRATE_POLICY` carries an ORDERED LAYER LIST, and every layer must bind.
//
// `config.test.ts` already pins the resolver: a delimiter-joined env var becomes
// N `policyPaths`, and overriding a config `policy` array produces a warning. Both
// of those assert a RETURN VALUE. Neither shows that the layers reach the engine,
// and neither shows that a human ever sees the warning.
//
// That distinction is the whole point of the mechanism. Policy layers only NARROW,
// so a layer that is resolved but never composed does not fail loudly -- it applies
// the wider policy and succeeds. The failure mode of "layer 2 was dropped" is a
// deploy that works, which is exactly the failure mode nobody investigates.
//
// So this file asserts ENFORCEMENT, through the real CLI, against a real database:
//
//   root only            -> the drop APPLIES        (control)
//   root + narrowing     -> the drop is REFUSED     (layer 2 bound)
//
// The control arm is not optional. Without it, the refusal arm would pass on a
// build where drops never work at all, or where a malformed layer path silently
// refused everything -- the two ways this test could report a false green.
//
// BOTH ENFORCEMENT MECHANISMS ARE COVERED. `safety.destructive_ops` is checked by
// PostgreSQL's SQL-text guard but over the IR op set on SQLite, so a layer that
// bound on one and not the other would be invisible to a single-dialect test.
//
// The second test covers the OTHER half: the env var REPLACES a config file's
// layers rather than adding to them. Since layers only narrow, dropping them
// widens effective policy -- so the CLI warns. This asserts the warning reaches
// STDERR, which is the only place it can do an operator any good.
//
// GATE: PG needs `ZERO_MIGRATE_TEST_PG_URL`. SQLite always runs.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { delimiter, dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_policy_layers";
const TABLE = "layered_rows";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** Layer 1. Grants the drop posture OUTRIGHT, so the control arm can succeed and
 *  any refusal must have come from a later layer. */
function rootLayer(namespace: string | null): string {
  const crossSchema = namespace
    ? `[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(namespace)}] }

`
    : "";
  return `policy_version = 1

${crossSchema}[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

/** Layer 2. Narrows the one knob layer 1 opened. `forbid` is the tightest posture
 *  (`Forbid` < `Warn` < `Allow`), so this is a legal narrowing rather than a widening
 *  an untrusted layer would be refused for attempting. */
const NARROWING_LAYER = `policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "forbid"
scope = "all"
`;

function project(namespace: string | null): { work: string; root: string; narrow: string } {
  const work = mkdtempSync(join(HERE, "policylayers-"));
  mkdirSync(join(work, "migrations"));
  const root = join(work, "policy-root.toml");
  const narrow = join(work, "policy-narrow.toml");
  writeFileSync(root, rootLayer(namespace));
  writeFileSync(narrow, NARROWING_LAYER);
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_make.ts"),
    `import { table, t } from "zero-migrate";
export const name = "make_layered";
export default {
  up() {
    table("${TABLE}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return { work, root, narrow };
}

function addDrop(work: string): void {
  writeFileSync(
    join(work, "migrations", "20260102000000_drop.ts"),
    `import { table } from "zero-migrate";
export const name = "drop_layered";
export default { up() { table("${TABLE}").drop(); } };
`,
  );
}

/** Deliberately passes the policy ONLY through `ZERO_MIGRATE_POLICY`, never through
 *  `--policy`: the flag has higher precedence and would mask whatever the env var
 *  did, turning every arm below into a test of the flag. */
function apply(
  work: string,
  databaseUrl: string,
  namespace: string | null,
  policyEnv: string,
  extraEnv: NodeJS.ProcessEnv = {},
): Promise<{ code: number | null; text: string }> {
  const schemaArgs = namespace ? ["--schema", namespace] : [];
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", databaseUrl,
        "--registry", join(work, "registry.json"),
        ...schemaArgs,
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        env: {
          ...process.env,
          ZERO_MIGRATE_ADDON_PATH: ADDON_PATH,
          DATABASE_URL: "",
          ZERO_MIGRATE_POLICY: policyEnv,
          ...extraEnv,
        },
      },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) => resolvePromise({ code, text: `${out}\n${err}`.trim() }));
  });
}

/** The refusal must name the posture, not merely be a non-zero exit. A drop can
 *  fail for a dozen unrelated reasons, and any of them would satisfy "it threw". */
function assertNarrowed(result: { code: number | null; text: string }, where: string): void {
  assert.equal(result.code, 1, `${where}: the narrowing layer must refuse the drop; ${result.text}`);
  assert.match(
    result.text,
    /destructive|DESTRUCTIVE_OPS_FORBID/i,
    `${where}: the refusal must name the posture layer 2 tightened; ${result.text}`,
  );
}

test("both ZERO_MIGRATE_POLICY layers bind on SQLite", async () => {
  const { work, root, narrow } = project(null);
  const url = `sqlite:${join(work, "app.db")}`;
  try {
    const made = await apply(work, url, null, root);
    assert.equal(made.code, 0, `the table must be created under the root layer; ${made.text}`);

    addDrop(work);
    // Layer 2 narrows `allow` to `forbid`; if it were dropped on the way to the
    // engine, this drop would simply succeed.
    assertNarrowed(await apply(work, url, null, [root, narrow].join(delimiter)), "SQLite");

    // Control: the SAME drop under layer 1 alone. This is what proves the refusal
    // above came from the narrowing rather than from the drop being broken.
    const dropped = await apply(work, url, null, root);
    assert.equal(dropped.code, 0, `the root layer alone must permit the drop; ${dropped.text}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("both ZERO_MIGRATE_POLICY layers bind on PostgreSQL", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("policylayers_pg");
  const { work, root, narrow } = project(namespace);
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const made = await apply(work, pgUrl(), namespace, root);
    assert.equal(made.code, 0, `the table must be created under the root layer; ${made.text}`);

    addDrop(work);
    // PostgreSQL enforces this posture in the SQL-TEXT guard rather than over the
    // IR op set, so this arm exercises a different mechanism than the SQLite one.
    assertNarrowed(
      await apply(work, pgUrl(), namespace, [root, narrow].join(delimiter)),
      "PostgreSQL",
    );

    const dropped = await apply(work, pgUrl(), namespace, root);
    assert.equal(dropped.code, 0, `the root layer alone must permit the drop; ${dropped.text}`);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

/** The env var REPLACES a config file's `policy` array; it does not add to it.
 *  Layers only narrow, so replacing three with one WIDENS effective policy. The
 *  resolver computes a warning for exactly this -- and a warning nobody prints is
 *  not a warning, which is what this arm checks. */
test("the CLI prints the warning when ZERO_MIGRATE_POLICY replaces a config policy", async () => {
  const { work, root, narrow } = project(null);
  try {
    writeFileSync(
      join(work, "zero-migrate.toml"),
      `[env.dev]
policy = [${JSON.stringify(root)}, ${JSON.stringify(narrow)}]
`,
    );
    // One env layer against the config's two.
    const result = await apply(work, `sqlite:${join(work, "app.db")}`, null, root);
    assert.equal(result.code, 0, `the create must still apply; ${result.text}`);
    assert.match(
      result.text,
      /ZERO_MIGRATE_POLICY \(1 layer\) overrides the 2-layer policy/,
      `the operator must be told which layers were dropped; ${result.text}`,
    );
    assert.match(
      result.text,
      /widen effective policy/,
      `and told what that means, since a dropped layer only ever loosens; ${result.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
