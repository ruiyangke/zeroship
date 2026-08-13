// `--json` is the machine-readable contract, so the things that break a pipeline
// are the things worth asserting: stdout must be exactly one JSON document, and
// the flag must not be accepted where there is no document to emit.
//
// The help text makes the first claim explicitly - engine diagnostics "never
// touch the single JSON document --json writes to stdout" - and the second is
// implicit in the usage block, which lists `[--json]` for lint, plan, rollback,
// status and history, and omits it for new, apply and resolve.
//
// The second claim was FALSE until this file. `--json` was accepted everywhere.
// On `apply` it did nothing: the command succeeded, stdout carried the human
// summary, and a pipeline piping that into a parser got a syntax error naming
// the summary rather than the flag. Every other verb-inappropriate flag is
// refused by name - `--dialect`, `--explain`, `--strict`, `--commit`,
// `--registry`, `--policy`, six of them - so this was the one that did not
// follow the CLI's own pattern.
//
// STDOUT PURITY IS CHECKED WITH LOGGING ON. Off, it proves little: nothing was
// trying to write. `ZERO_MIGRATE_LOG=1` is the setting the help text is actually
// promising about, and it is the one a debugging operator turns on while their
// pipeline is still parsing the output.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL` for the live verbs; the refusal arms are
// offline.

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
const OWNER_APP = "app_json_contract";

/**
 * Every command that accepts `--json`. Five come from the usage block; `version`
 * does not appear there as a verb - the usage shows the `--version` FLAG form -
 * and `docs/cli.md` documents it separately ("`--json` without `--verbose`
 * leaves `version` output unchanged"). Deriving this list from the usage block
 * alone missed it, and the existing `version` test caught the omission.
 */
const JSON_VERBS = ["lint", "plan", "status", "history", "rollback", "version"] as const;
/** And the ones it does not. */
const NON_JSON_VERBS = ["new", "apply", "resolve"] as const;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "jsonc-"));
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
`,
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_create.ts"),
    `import { table, t } from "zero-migrate";
export const name = "create_t1";
export default {
  up() {
    table("t1").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

function run(
  work: string,
  argv: string[],
  extraEnv: Record<string, string> = {},
): { code: number | null; out: string; err: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: {
        ...process.env,
        ZERO_MIGRATE_ADDON_PATH: ADDON_PATH,
        DATABASE_URL: "",
        ...extraEnv,
      },
    },
  );
  return { code: result.status, out: result.stdout ?? "", err: result.stderr ?? "" };
}

test("--json is refused by the commands that have no machine-readable reply", () => {
  const work = project("unused");
  try {
    for (const verb of NON_JSON_VERBS) {
      const result = run(work, [verb, "--json"]);
      assert.equal(result.code, 1, `${verb} --json must be refused`);
      assert.match(
        result.err,
        /flag --json is only valid with lint, plan, status, rollback, history, or version/,
        `${verb}: the refusal must name the commands that do accept it; got ${result.err}`,
      );
      // Refused at argument parsing, before anything else is reported. Without
      // this, a refusal that happened to fire after a connection attempt would
      // still pass the assertion above while being useless in a pipeline.
      assert.equal(result.out, "", `${verb}: a refused flag must print nothing to stdout`);
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("--json is accepted by every command the usage block marks with it", () => {
  // The control. "Refused on three verbs" also holds for a build where --json
  // was refused everywhere, which would be a far worse regression and would look
  // identical in the test above.
  const work = project("unused");
  try {
    for (const verb of JSON_VERBS) {
      const result = run(work, [verb, "--json"]);
      assert.doesNotMatch(
        result.err,
        /flag --json is only valid/,
        `${verb} must accept --json`,
      );
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("with engine logging on, stdout is still exactly one JSON document", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const schema = uniqueNamespace("jsonc");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    // A real deploy first, so `status` and `history` have something to report and
    // the documents below are not trivially empty.
    const applied = run(work, ["apply", "--approve", "--database-url", pgUrl(), "--schema", schema]);
    assert.equal(applied.code, 0, `apply must succeed; ${applied.err}`);

    for (const verb of ["plan", "status", "history"] as const) {
      const result = run(
        work,
        [verb, "--json", "--database-url", pgUrl(), "--schema", schema],
        { ZERO_MIGRATE_LOG: "1" },
      );
      assert.equal(result.code, 0, `${verb} must succeed; ${result.err}`);
      // The claim: parseable as ONE document, with logging turned on.
      let parsed: unknown;
      assert.doesNotThrow(() => {
        parsed = JSON.parse(result.out);
      }, `${verb} --json with ZERO_MIGRATE_LOG=1 must emit one JSON document; got: ${result.out.slice(0, 200)}`);
      assert.ok(
        parsed !== null && typeof parsed === "object",
        `${verb}: the document must be an object or array`,
      );
    }
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
