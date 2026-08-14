// The host allowlist must constrain the host the DRIVER reaches, not the one the
// URL's authority happens to spell.
//
// `--host-allowlist` refuses a connection whose host is not approved, and it
// decides that with WHATWG `new URL(u).hostname`. But `pg` re-parses the
// connection string with its own parser, and a PostgreSQL URI may carry a `host`
// QUERY PARAMETER that overrides the authority. Two parsers, two answers, and the
// one that decides is not the one that checks:
//
//   postgres://user:pw@approved.example:5432/db?host=somewhere.else
//   ^ the allowlist sees `approved.example`   ^ the driver connects HERE
//
// So an allowlist naming only approved hosts admitted a connection to an arbitrary
// one. The control is what makes that a bypass rather than a curiosity: the SAME
// URL with the parameter removed cannot resolve `allowed.invalid` at all, so a
// successful connection can only have come from the parameter.
//
// This is the whole point of the control. An allowlist that refuses everything
// would satisfy any "it was refused" assertion, and an allowlist that is never
// consulted would satisfy any "it connected" assertion. Both directions are
// pinned here.
//
// MYSQL IS NOT AFFECTED and is covered anyway, because "the other driver happens
// to ignore that parameter today" is a property worth pinning rather than
// assuming: mysql2 refuses the same URL with `getaddrinfo ENOTFOUND`.
//
// `.invalid` is a reserved TLD (RFC 2606) and can never resolve, which is what
// makes the negative control trustworthy rather than dependent on the test host's
// DNS.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL`, `ZERO_MIGRATE_MYSQL_URL`.

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
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_allowlist";
const TABLE = "allowlist_rows";

/** Reserved by RFC 2606: guaranteed never to resolve, on any test host. */
const UNREACHABLE = "allowed.invalid";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "allowlist-"));
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
  databaseUrl: string,
  allowlist: string,
  namespace: string | null,
): Promise<{ code: number | null; text: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", databaseUrl,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--host-allowlist", allowlist,
        ...(namespace ? ["--schema", namespace] : []),
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
    child.on("close", (code) => resolvePromise({ code, text: `${out}\n${err}`.trim() }));
  });
}

/** Split the authority off a live URL so the test can rebuild it with a different
 *  host while keeping the real credentials, port, and database. */
function parts(url: string): { credentials: string; hostPort: string; tail: string } {
  const parsed = new URL(url);
  const credentials = `${parsed.username}:${parsed.password}`;
  return {
    credentials,
    hostPort: `${parsed.hostname}:${parsed.port}`,
    tail: parsed.pathname,
  };
}

test("a `host` query parameter cannot escape the PostgreSQL allowlist", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const { credentials, hostPort, tail } = parts(pgUrl());
  const realHost = hostPort.split(":")[0];
  const port = hostPort.split(":")[1];
  const namespace = uniqueNamespace("allowlist_pg");
  const work = project();
  try {
    // The schema is created up front ON PURPOSE. Everything except the allowlist
    // must be VALID, or the run fails for an unrelated reason and the assertion
    // passes without testing anything -- which is exactly what happened on the
    // first draft of this test: a missing schema made the smuggled connection
    // fail with "schema does not exist" AFTER it had already reached the host the
    // allowlist forbade.
    await client.query(`CREATE SCHEMA "${namespace}"`);

    // The authority names the approved host; the parameter names the real one.
    const smuggled = `postgres://${credentials}@${UNREACHABLE}:${port}${tail}?host=${realHost}`;
    const result = await apply(work, smuggled, UNREACHABLE, namespace);

    // The decisive assertion is the DATABASE, not the exit code: the question is
    // whether the migration reached a host the operator did not approve.
    const { rows } = await client.query(
      `SELECT table_name FROM information_schema.tables WHERE table_schema = $1`,
      [namespace],
    );
    assert.deepEqual(
      rows,
      [],
      `the migration reached a host outside the allowlist and applied there; ${result.text}`,
    );
    assert.equal(
      result.code,
      1,
      `a URL whose driver-visible host is outside the allowlist must be refused; ${result.text}`,
    );
    assert.match(
      result.text,
      /allowlist/i,
      `the refusal must come from the allowlist, not from something incidental; ${result.text}`,
    );
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

test("CONTROL: the same URL without the parameter cannot connect at all", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const { credentials, hostPort, tail } = parts(pgUrl());
  const port = hostPort.split(":")[1];
  const work = project();
  try {
    // Proves the smuggled arm's connection could ONLY have come from `?host=`.
    // `allowed.invalid` is unresolvable, so if this ever succeeds the reserved-TLD
    // assumption has broken and the bypass test above proves nothing.
    const result = await apply(
      work,
      `postgres://${credentials}@${UNREACHABLE}:${port}${tail}`,
      UNREACHABLE,
      null,
    );
    assert.equal(result.code, 1, `the unreachable authority must fail; ${result.text}`);
    assert.match(
      result.text,
      /ENOTFOUND|EAI_AGAIN|getaddrinfo/i,
      `and it must fail by not resolving, not for some other reason; ${result.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("CONTROL: an allowlisted host still applies", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("allowlist_ok");
  const work = project();
  const realHost = new URL(pgUrl()).hostname;
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    // Without this the refusals above would also pass on a build where the
    // allowlist rejected everything.
    const result = await apply(work, pgUrl(), realHost, namespace);
    assert.equal(result.code, 0, `an allowlisted host must still apply; ${result.text}`);
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

test("CONTROL: a non-allowlisted host is refused", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const work = project();
  try {
    // The plain case the allowlist was built for; proves it is consulted at all.
    const result = await apply(work, pgUrl(), "somewhere.else.invalid", null);
    assert.equal(result.code, 1, `a host outside the allowlist must be refused; ${result.text}`);
    assert.match(result.text, /allowlist/i, `naming the allowlist; ${result.text}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("MySQL ignores a `host` query parameter, and the allowlist still holds", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const { credentials, hostPort, tail } = parts(String(MYSQL_URL));
  const realHost = hostPort.split(":")[0];
  const port = hostPort.split(":")[1] || "3306";
  const work = project();
  try {
    const smuggled = `mysql://${credentials}@${UNREACHABLE}:${port}${tail}?host=${realHost}`;
    const result = await apply(work, smuggled, UNREACHABLE, null);
    // mysql2 does not honour the parameter, so this fails either by refusing the
    // driver-visible host or by failing to resolve. Both are acceptable; silently
    // CONNECTING is not, and that is what this arm forbids.
    assert.equal(result.code, 1, `MySQL must not reach the smuggled host; ${result.text}`);
    assert.doesNotMatch(
      result.text,
      /applied/i,
      `and must not have applied the migration; ${result.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
