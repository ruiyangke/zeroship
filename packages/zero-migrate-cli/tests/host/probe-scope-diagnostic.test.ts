// The refusal an operator meets when their policy scopes no schema at all.
//
// A guarded catalog probe is authorized against the effective schema scope
// (`authorize_existence_guard_schema`). That scope is built ONLY from
// `schema.cross_schema` grant includes (`owned_schemas_from_effective`), so a
// policy that never grants that key resolves to `SchemaScope::Single("")` — a pin
// to the empty string, which permits no real schema, the project's own included.
//
// The message used to stop at "which the effective policy schema scope does not
// permit". That is true and useless: it describes an EXCLUSION, and the operator
// goes looking through their policy for one they never wrote. The common cause is
// the opposite — nothing was scoped at all.
//
// SQLite is the cheapest place to meet it. A table create emits no probe, so it
// succeeds; adding an index to that same table does probe, so one migration
// straddles the boundary and the refusal lands on a purely local operation.
//
// WHAT THIS PINS is the remedy, not the prose: the message must name the key to
// grant AND the schema to include, because those two facts are what turn the
// error into an action. It deliberately does not assert the whole sentence, which
// would break on any rewording.
//
// NOT pinned here: whether requiring the grant for a local operation is right at
// all. That is a scope-model question recorded separately; this file only says
// that whatever the rule is, the refusal explains itself.
//
// GATE: none. SQLite needs no server.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
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

const OWNER_APP = "app_probe_scope";
const TABLE = "probe_scope_rows";

/** `crossSchema: false` is the case under test: a policy that scopes no schema. */
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
scope = "all"
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
  // The index add is what probes. The create alone would succeed either way, so
  // both ops are present to keep the contrast inside ONE migration.
  writeFileSync(
    join(work, "migrations", "20260101000000_make.ts"),
    `import { table, t } from "zero-migrate";
export const name = "make_rows";
export default {
  up() {
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

test("an out-of-scope probe refusal names the grant and the schema to add", async () => {
  const work = project(false);
  try {
    const refused = await apply(work);
    assert.equal(refused.code, 1, `the probe must be refused; ${refused.text}`);
    assert.match(
      refused.text,
      /existence-guard probe/,
      `and it must be the probe authorization that refuses: ${refused.text}`,
    );

    // THE REMEDY, which is the whole point. Either half alone leaves the operator
    // stuck: the key without the schema, or the schema without the key.
    assert.match(
      refused.text,
      /schema\.cross_schema/,
      `the refusal must name the key to grant: ${refused.text}`,
    );
    assert.match(
      refused.text,
      /"public"/,
      `and the schema to include: ${refused.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

/** The control. Following the printed remedy must actually work — a diagnostic
 *  that names a fix which does not fix it is worse than a terse one. */
test("following that remedy lets the same migration apply", async () => {
  const work = project(true);
  try {
    const applied = await apply(work);
    assert.equal(
      applied.code,
      0,
      `granting exactly what the refusal asked for must apply the migration; ${applied.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
