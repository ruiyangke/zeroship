// Every flag `--help` shows for a verb is accepted there, and every flag it does
// NOT show is refused there.
//
// The CLI already refuses a misplaced `--dialect`, `--explain`, `--strict`,
// `--json`, `--registry`, `--policy`, `--journal`, and the rollback target flags,
// each with a message naming the commands the flag belongs to. The reason is
// written into `cli.ts` beside the `--json` guard:
//
//   "the CLI already tells an operator which commands a flag belongs to, and this
//    was the one that did not."
//
// `--approve` was the one that did not. It was accepted by `new`, `lint`, `plan`,
// `status` and `history`, where it does nothing at all -- and it is the APPROVAL
// flag for destructive work, so silently accepting it is the worst member of the
// class to leave open. `zero-migrate plan --approve` reads like pre-approving the
// plan; it approves nothing, and nothing said so.
//
// THE MATRIX IS DERIVED FROM `--help`, NOT HAND-WRITTEN. A hand-written list would
// be a second copy of the same claim, and the two would drift the way any two
// copies do. Parsing the usage block means this test asserts "the binary does what
// its own help says", which is the property worth having: adding a flag to a verb's
// usage line without wiring it, or wiring it without documenting it, fails here.
//
// A flag is judged REFUSED by the "is only valid with" message rather than by exit
// status, because every one of these runs exits non-zero for its own domain
// reasons -- a missing database, an unresolvable migration -- and an exit code
// cannot tell those apart from a flag rejection.
//
// GATE: none. Every case is an argument-parsing decision made before any
// connection is opened.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const CLI_SRC = resolve(HERE, "../../src/cli.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

/** Flags whose per-verb validity the CLI enforces, with a usable value for the
 *  ones that take one. `null` marks a boolean. */
const FLAGS: Readonly<Record<string, string | null>> = {
  "--registry": "REGISTRY",
  "--policy": "POLICY",
  "--journal": "JOURNAL",
  "--dialect": "postgres",
  "--json": null,
  "--strict": null,
  "--explain": null,
  "--approve": null,
};

const VERBS = [
  "new",
  "lint",
  "plan",
  "apply",
  "status",
  "rollback",
  "resolve",
  "history",
] as const;

function scratch(): string {
  const work = mkdtempSync(join(HERE, "flagverb-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(join(work, "policy.toml"), "policy_version = 1\n");
  writeFileSync(join(work, "registry.json"), "{}");
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  schema() {
    table("fv_t").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

/** What the usage block claims for one verb. */
function flagsInUsage(verb: string): ReadonlySet<string> {
  const source = readFileSync(CLI_SRC, "utf8");
  const start = source.indexOf("Usage:");
  const end = source.indexOf("zero-migrate --version", start);
  assert.ok(start !== -1 && end !== -1, "the usage block must be findable in cli.ts");
  const line = source
    .slice(start, end)
    .split("\n")
    .find((candidate) => candidate.trim().startsWith(`zero-migrate ${verb} `));
  assert.ok(line !== undefined, `--help must document the ${verb} command`);
  return new Set(line.match(/--[a-z-]+/g) ?? []);
}

/** True when the CLI does NOT reject the flag as misplaced for this verb. */
function accepts(work: string, verb: string, flag: string): { ok: boolean; text: string } {
  const value = FLAGS[flag];
  const resolved =
    value === "REGISTRY"
      ? join(work, "registry.json")
      : value === "POLICY"
        ? join(work, "policy.toml")
        : value === "JOURNAL"
          ? join(work, "j.sqlite")
          : value;
  const args = ["--import", "tsx", CLI_BIN, verb];
  if (verb === "new") args.push("scratch_name");
  if (verb === "resolve") args.push("some_migration");
  args.push(flag);
  if (resolved !== null) args.push(resolved);
  // A plausible target for every verb, so the run fails for its own reasons rather
  // than for a missing URL, which would mask the flag verdict.
  args.push("--dir", join(work, "migrations"), "--database-url", `sqlite:${join(work, "a.db")}`);

  const result = spawnSync(process.execPath, args, {
    cwd: work,
    encoding: "utf8",
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
  const text = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  return { ok: !/is only valid with|are only valid with/.test(text), text: text.trim() };
}

test("the CLI accepts exactly the flags --help shows for each verb", () => {
  const work = scratch();
  try {
    const mismatches: string[] = [];
    for (const verb of VERBS) {
      const documented = flagsInUsage(verb);
      for (const flag of Object.keys(FLAGS)) {
        const claimed = documented.has(flag);
        const { ok, text } = accepts(work, verb, flag);
        if (claimed !== ok) {
          mismatches.push(
            `${verb} ${flag}: --help ${claimed ? "shows" : "omits"} it but the CLI ` +
              `${ok ? "accepts" : "refuses"} it${ok ? "" : ` (${text.split("\n").filter(Boolean).pop()})`}`,
          );
        }
      }
    }
    assert.deepEqual(mismatches, [], mismatches.join("\n"));
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

/** The specific case that motivated the matrix, asserted on its own so a
 *  regression names the flag rather than appearing as one row of a list. */
test("--approve is refused by the verbs that approve nothing", () => {
  const work = scratch();
  try {
    for (const verb of ["new", "lint", "plan", "status", "history"] as const) {
      const { ok, text } = accepts(work, verb, "--approve");
      assert.equal(
        ok,
        false,
        `${verb} approves nothing, so --approve must be refused rather than ` +
          `silently accepted; got: ${text.split("\n").filter(Boolean).pop()}`,
      );
      assert.match(
        text,
        /apply, rollback, or resolve/,
        `and the refusal must name the commands it does belong to; got: ${text}`,
      );
    }
    // The control: the three verbs that DO consume it must still take it, or the
    // refusal above would be indistinguishable from breaking the flag outright.
    for (const verb of ["apply", "rollback", "resolve"] as const) {
      const { ok, text } = accepts(work, verb, "--approve");
      assert.equal(ok, true, `${verb} must still accept --approve; got: ${text}`);
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
