// The preview an author is told to read before trusting a guard.
//
// A guard (`ifNotExists`/`ifExists`) is not a native `IF NOT EXISTS` clause. The
// engine probes the live catalog and then runs, no-ops, or fails on drift - and
// what that means differs per dialect, sharply enough that
// `docs/writing-migrations.md` tells authors not to assume and to read the
// preview label instead.
//
// That instruction named a `--sql` preview. There is no `--sql` flag on any verb:
// `zero-migrate plan --sql` answers `unknown flag --sql`. So the one sentence
// telling an author to verify rather than guess pointed at a command that does
// not exist. The real one is `lint --explain --dialect <target>`.
//
// The label is the whole point, and it is what nothing tested. `cli.test.ts` has
// a `lint --explain` arm, but it asserts the rendered `CREATE TABLE` and the
// dialect banner - both of which appear for an UNGUARDED migration too. A build
// that dropped guard labelling entirely would keep it green.
//
// So this asserts the label, on all three dialects, and asserts that MySQL's
// differs. Checking only that some label appears would pass on a build that
// emitted one fixed string everywhere, which is precisely the "assume" the
// documentation is warning against.
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

function project(guarded: boolean): string {
  const work = mkdtempSync(join(HERE, "guardpreview-"));
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
export const name = "guarded";
export default {
  up() {
    table("users").create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],${guarded ? "\n      ifNotExists: true," : ""}
    });
  },
};
`,
  );
  return work;
}

function run(work: string, argv: string[]): { status: number | null; output: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--owner-app", "app_guard_preview",
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

test("lint --explain labels a guarded statement on every dialect", () => {
  const work = project(true);
  try {
    for (const dialect of ["postgres", "mysql", "sqlite"] as const) {
      const { status, output } = run(work, ["lint", "--explain", "--dialect", dialect]);
      assert.equal(status, 0, `lint must succeed on ${dialect}; ${output}`);
      assert.match(
        output,
        /\[runtime-resolved\] createTable "users" \(ifNotExists\)/,
        `${dialect}: the guarded statement must be labelled as runtime-resolved`,
      );
      assert.match(
        output,
        /catalog-probed at apply \(run \/ satisfied-noop \/ fail-drift\)/,
        `${dialect}: the label must say what the probe does`,
      );
      // The rendered DDL is the bare statement, not a native IF NOT EXISTS -
      // which is the misreading the label exists to prevent.
      assert.doesNotMatch(
        output,
        /CREATE TABLE IF NOT EXISTS/i,
        `${dialect}: a guard must not render as a native IF NOT EXISTS clause`,
      );
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("the MySQL label carries a caveat the other two do not", () => {
  // Checking only that SOME label appears would pass on a build emitting one
  // fixed string everywhere. The documented reason to read the label at all is
  // that MySQL differs, so the difference is the property worth pinning.
  const work = project(true);
  try {
    const mysql = run(work, ["lint", "--explain", "--dialect", "mysql"]).output;
    assert.match(
      mysql,
      /MySQL column-type equality/,
      `the MySQL label must carry its own caveat; got: ${mysql}`,
    );
    for (const dialect of ["postgres", "sqlite"] as const) {
      assert.doesNotMatch(
        run(work, ["lint", "--explain", "--dialect", dialect]).output,
        /MySQL column-type equality/,
        `${dialect} must not carry MySQL's caveat`,
      );
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("an unguarded statement gets no runtime-resolved label", () => {
  // The control. Every assertion above would also hold for a build that labelled
  // every statement unconditionally, which would tell an author nothing about
  // whether their guard was understood.
  const work = project(false);
  try {
    const { status, output } = run(work, ["lint", "--explain", "--dialect", "postgres"]);
    assert.equal(status, 0, `lint must succeed; ${output}`);
    assert.match(output, /CREATE TABLE/i, "the statement is still rendered");
    // The bracketed LABEL, not the word: the summary footer names the count on
    // every preview, so matching the bare word would assert nothing.
    assert.doesNotMatch(
      output,
      /\[runtime-resolved\]/,
      "an unguarded statement must not be labelled runtime-resolved",
    );
    assert.match(
      output,
      /0 runtime-resolved/,
      "and the summary must count it as none",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("no verb accepts the --sql flag the documentation used to name", () => {
  // Pins the defect that prompted this file. If a `--sql` preview is ever added,
  // this fails and the documentation can name it again - deliberately, rather
  // than by describing a flag that was never there.
  const work = project(true);
  try {
    for (const verb of ["plan", "lint", "apply", "status"]) {
      const { output } = run(work, [verb, "--sql"]);
      assert.match(
        output,
        /unknown flag --sql/,
        `${verb} must reject --sql; got: ${output}`,
      );
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
