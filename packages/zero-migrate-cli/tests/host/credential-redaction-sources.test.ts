// A password must not reach the operator's terminal, whichever source carried it.
//
// `cli.test.ts` already pins the main shape: a `--database-url` with inline
// credentials pointed at an unreachable host produces no URL, user, or password in
// stderr. This file covers the paths that one does NOT, because they differ
// structurally rather than cosmetically:
//
//   ZERO_MIGRATE_URL    the URL is not a flag, so `args.databaseUrl` is assigned
//                       only DURING config resolution -- and the catch in `main`
//                       reads that field to know what to redact. An error thrown
//                       before the assignment has no URL to split on and falls back
//                       to the generic URL-shaped-token pass.
//   config file `url`   same timing, and the URL never appears on the command line
//                       at all, so a leak here could not come from argv echo.
//   live auth failure   the message is written by the SERVER, not the driver. An
//                       authentication error is the likeliest place for a password
//                       to be echoed, and it is the one case an unreachable host
//                       can never produce -- the connection has to get far enough
//                       to be rejected.
//   percent-encoded     the URL carries `p%40ssw0rd`; anything that decodes it sees
//                       `p@ssw0rd`. The redactor splits on the string it was given,
//                       so the decoded form is a different needle. Both are checked.
//
// THE CONTROL IS THE POINT OF THE FILE. "The secret was absent" is what a test
// prints when the secret is absent AND when the test cannot see secrets at all --
// a truncated capture, a swallowed stream, a typo'd needle. So the last test puts
// the SAME string somewhere nothing redacts it (an owner-app ID) and requires it to
// appear. If that test ever fails, every clean result above became meaningless.
//
// GATE: the live-auth arm needs `ZERO_MIGRATE_TEST_PG_URL`. The rest always run.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
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
const OWNER_APP = "app_redaction";
const TABLE = "redaction_rows";

/** Distinctive enough that any appearance in output is unambiguous. */
const SECRET = "hunter2SECRET";
/** What the URL literally contains, and what a decoder would turn it into. */
const ENCODED_IN_URL = "p%40ssw0rd";
const ENCODED_DECODED = "p@ssw0rd";
/** Unresolvable by construction: `.invalid` is reserved and never resolves. */
const DEAD_HOST = "nonexistent.invalid:5432";

function project(): string {
  const work = mkdtempSync(join(HERE, "redaction-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_base.ts"),
    `import { table, t } from "zero-migrate";
export const name = "base";
export default {
  schema() {
    table("${TABLE}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

function apply(
  work: string,
  options: { url?: string; env?: NodeJS.ProcessEnv; ownerApp?: string },
): Promise<{ code: number | null; text: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        ...(options.url ? ["--database-url", options.url] : []),
        "--owner-app", options.ownerApp ?? OWNER_APP,
      ],
      {
        cwd: work,
        env: {
          ...process.env,
          ZERO_MIGRATE_ADDON_PATH: ADDON_PATH,
          DATABASE_URL: "",
          ...options.env,
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

/** Requires a real failure first: a run that somehow SUCCEEDED would print no
 *  error at all and would trivially contain no secret. */
function assertNoLeak(
  result: { code: number | null; text: string },
  needles: readonly string[],
  where: string,
): void {
  assert.equal(result.code, 1, `${where}: the run must actually fail; ${result.text}`);
  for (const needle of needles) {
    assert.equal(
      result.text.includes(needle),
      false,
      `${where}: ${JSON.stringify(needle)} reached the operator's terminal; ${result.text}`,
    );
  }
}

test("a password from ZERO_MIGRATE_URL is never printed", async () => {
  const work = project();
  try {
    const result = await apply(work, {
      env: { ZERO_MIGRATE_URL: `postgres://appuser:${SECRET}@${DEAD_HOST}/db` },
    });
    assertNoLeak(result, [SECRET], "ZERO_MIGRATE_URL");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("a password from a config file URL is never printed", async () => {
  const work = project();
  try {
    writeFileSync(
      join(work, "zero-migrate.toml"),
      `[env.dev]\nurl = "postgres://appuser:${SECRET}@${DEAD_HOST}/db"\n`,
    );
    assertNoLeak(await apply(work, {}), [SECRET], "config url");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("a password is never printed when the error precedes URL resolution", async () => {
  const work = project();
  try {
    // An empty schema is refused inside config resolution, so this error is raised
    // BEFORE `args.databaseUrl` has been assigned from the environment -- the catch
    // has no URL to split on and must still not leak.
    const result = await apply(work, {
      env: {
        ZERO_MIGRATE_URL: `postgres://appuser:${SECRET}@${DEAD_HOST}/db`,
        ZERO_MIGRATE_SCHEMA: "",
      },
    });
    assertNoLeak(result, [SECRET], "pre-resolution failure");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("neither form of a percent-encoded password is printed", async () => {
  const work = project();
  try {
    const result = await apply(work, {
      url: `postgres://appuser:${ENCODED_IN_URL}@${DEAD_HOST}/db`,
    });
    // The encoded form is what the redactor was handed; the decoded form is what
    // anything that parses the URL would actually hold.
    assertNoLeak(result, [ENCODED_IN_URL, ENCODED_DECODED], "percent-encoded password");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("a live authentication failure does not echo the password", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const work = project();
  try {
    // Same reachable server as the rest of the suite, deliberately wrong
    // credentials, so the REJECTION is written by PostgreSQL itself.
    const authority = pgUrl().replace(/^[a-z+]+:\/\/[^@]*@/i, "");
    const result = await apply(work, { url: `postgres://appuser:${SECRET}@${authority}` });
    assertNoLeak(result, [SECRET], "live auth failure");
    assert.match(
      result.text,
      /authentication failed|password/i,
      `the arm must reach a real authentication rejection; ${result.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

/** The instrument check. Same string, same capture, same needle -- placed where
 *  nothing redacts it. If this stops failing to find the secret, every assertion
 *  above has quietly stopped meaning anything. */
test("CONTROL: the capture can see the secret when nothing redacts it", async () => {
  const work = project();
  try {
    const result = await apply(work, {
      url: `sqlite:${join(work, "app.db")}`,
      // An owner-app ID is not a credential, so it is echoed verbatim by the
      // ownership refusal.
      ownerApp: SECRET,
    });
    assert.equal(result.code, 1, `the control must fail to produce a message; ${result.text}`);
    assert.ok(
      result.text.includes(SECRET),
      "the capture cannot see the secret even when it IS present, so every " +
        `clean result in this file is meaningless; ${result.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
