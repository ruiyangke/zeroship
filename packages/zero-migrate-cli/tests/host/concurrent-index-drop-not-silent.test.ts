// `concurrently: true` on an index drop must not be silently ignored.
//
// F651. Two docs state the capability plainly:
//
//   writing-migrations.md:693  "Concurrent index drop is not supported."
//   dialects.md:291            "Concurrent index drop is unsupported."
//
// They are accurate about the ENGINE. The DSL disagreed with them: `concurrently`
// is in `INDEX_DROP_KEYS`, so `table(x).index(n).drop({ concurrently: true })`
// passes `rejectUnknownKeys`, lints `ok`, and then emits
//
//   DROP INDEX "s"."cc_ix";
//
// with no `CONCURRENTLY`. The flag is accepted and discarded.
//
// THAT IS THE FAILURE MODE THIS PROJECT SAYS IT DOES NOT HAVE. `types.ts` states:
// "None of the above is ever a silent no-op — an unsupported spec fails closed at
// lower time." A plain `DROP INDEX` takes ACCESS EXCLUSIVE on the table. An author
// who wrote `concurrently: true` specifically to avoid that lock gets the lock
// anyway, with no error and no warning, and finds out in production.
//
// The sibling op is the proof that fail-closed is the intended behaviour here:
// `index(n).add({ concurrently: true })` is REFUSED outright, naming its accepted
// keys. Add refusing and drop ignoring is not a considered asymmetry; it is one op
// forgetting to fail closed.
//
// I ORIGINALLY GOT THIS BACKWARDS, which is why the assertion is on emitted SQL.
// Reading `INDEX_DROP_KEYS` and seeing `concurrently` there, I recorded "drop
// supports it, add does not" as an asymmetry in the DSL surface. Membership in an
// accepted-keys list proves only that the unknown-key check passes -- never that
// the option reaches the database. The docs are what exposed the mistake.
//
// GATE: none. This is an offline authoring decision.

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

const OWNER_APP = "app_ccdrop";

function project(dropArgs: string): string {
  const work = mkdtempSync(join(HERE, "ccdrop-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ cc_t: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  schema() {
    table("cc_t").create({
      columns: { id: t.int().notNull(), e: t.text() },
      primaryKey: ["id"],
    });
    table("cc_t").index("cc_ix").add({ on: ["e"] });
    table("cc_t").index("cc_ix").drop(${dropArgs});
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

test("a concurrent index drop is refused, not accepted and ignored", () => {
  const work = project(`{ concurrently: true }`);
  try {
    const linted = lint(work);
    assert.equal(
      linted.code,
      1,
      `an unsupported safety flag must FAIL CLOSED. Accepting it and emitting a ` +
        `plain DROP INDEX gives the author the ACCESS EXCLUSIVE lock they wrote ` +
        `the flag to avoid, silently; ${linted.text}`,
    );
    assert.match(
      linted.text,
      /concurrent/i,
      `and the refusal must NAME the flag, so the author knows which option is ` +
        `unsupported rather than hunting a generic rejection; ${linted.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("CONTROL: an ordinary index drop still lints clean", () => {
  // Without this, the refusal above is equally consistent with having broken
  // index drops altogether.
  const work = project(`{}`);
  try {
    const linted = lint(work);
    assert.equal(linted.code, 0, `a plain index drop must still work; ${linted.text}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("the key is gone entirely, so concurrently:false is refused too", () => {
  // I first wrote this expecting `false` to be ACCEPTED, on the reasoning that
  // explicitly declining concurrency describes what the engine already does. The
  // implementation says otherwise and is right: `index(n).add(...)` has no
  // `concurrently` key at all, so drop matching it exactly is the consistent
  // shape. Keeping the key to accept one value would leave a vestigial option
  // that does nothing, has to be documented, and invites the same confusion
  // again. Refusing the KEY rather than the VALUE is the simpler contract.
  const work = project(`{ concurrently: false }`);
  try {
    const linted = lint(work);
    assert.equal(
      linted.code,
      1,
      `the option does not exist on this op, whatever its value; ${linted.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
