// Redacting the username must not eat the word everywhere it appears.
//
// `safeErrorMessage` replaced the URL's username by plain substring match, so a
// username of `postgres` -- the DEFAULT PostgreSQL superuser, and the name of the
// dialect the engine reports -- rewrote its own diagnostics:
//
//   [OP_INVALID kind=op op_index=0 dialect=<redacted user>]
//
// Over-redaction never leaks, so this was a quality defect rather than a security
// one. But `dialect=…` is the single most useful token in a three-dialect engine's
// error: it says which target refused. Replacing it with `<redacted user>` leaves a
// message reading as though a credential appeared where a dialect name belongs.
//
// The username is now replaced only where the surrounding text marks it as a
// CREDENTIAL -- `user@host`, `user:password`, or a libpq-style `user=name`. A bare
// word elsewhere is left alone.
//
// BOTH DIRECTIONS ARE ASSERTED, because a fix for over-redaction is one careless
// edit away from under-redaction. The password must still be absent from the same
// message that now keeps `dialect=postgres`. `credential-redaction-sources.test.ts`
// covers the leak question across URL sources; this file covers the one message
// where the two requirements meet.
//
// THE PASSWORD IS DELIBERATELY NOT TREATED THIS WAY. A password is a real secret,
// so redacting it wherever it appears is the correct posture even when it collides
// with an ordinary word -- over-redaction there is the safe failure. Only the
// username, which the URL and credential-pair rules already cover in every
// credential-shaped form, is narrowed.
//
// That asymmetry is why this test uses a PURPOSE-MADE ROLE rather than the shared
// test URL. The usual `postgres://postgres:postgres@…` has username EQUAL to
// password, so the password rule would eat the word first and the username rule
// could never be observed -- the test would fail for a reason it is not about. The
// role below has a name that appears in the diagnostic and a password that does
// not, which isolates exactly one rule.
//
// The failure is provoked with a real engine refusal rather than a synthetic
// string: `perRow.typeId(...)` into a generic `t.text()` column is refused because
// generic text carries no value-format contract, and that refusal quotes the table
// name. Using a real diagnostic means the test breaks if the engine stops naming
// the table at all, which is worth knowing too.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`, with rights to create a role.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
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
const OWNER_APP = "app_redpos";
const TABLE = "redpos_rows";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** A migration whose backfill the engine refuses by naming the dialect. */
function project(): string {
  const work = mkdtempSync(join(HERE, "redpos-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), tid: t.text() },
      primaryKey: ["id"],
    });
    table("${TABLE}").insert({ rows: [{ id: 1 }] });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `import { table, perRow } from "zero-migrate";
export const name = "b";
export default {
  up() {
    // Refused: a generic text column declares no value format for a TypeID.
    table("${TABLE}").backfill({
      set: { tid: perRow.typeId({ prefix: "order" }) },
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
    });
  },
};
`,
  );
  return work;
}

/** A role whose NAME is a word the diagnostic contains, with a password that is
 *  not. `TABLE` appears in the engine's refusal, so naming the role after it makes
 *  the collision exact and the isolation total. */
const ROLE = TABLE;
const ROLE_PASSWORD = "Zm0nlyForThisTest";

test("a username matching a word in the diagnostic does not eat that word", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const admin = new pg.Client({ connectionString: pgUrl() });
  await admin.connect();
  const namespace = uniqueNamespace("redpos");
  const work = project();
  let roleCreated = false;
  try {
    try {
      // Clear anything a previous run left owned by the role before dropping it,
      // or a stale role turns every later run into a silent skip.
      await admin.query(`DROP OWNED BY "${ROLE}" CASCADE`).catch(() => {});
      await admin.query(`DROP ROLE IF EXISTS "${ROLE}"`);
      await admin.query(
        `CREATE ROLE "${ROLE}" LOGIN PASSWORD '${ROLE_PASSWORD}' SUPERUSER`,
      );
      roleCreated = true;
    } catch (error) {
      ctx.skip(`cannot create the probe role: ${(error as Error).message}`);
      return;
    }
    await admin.query(`CREATE SCHEMA "${namespace}"`);

    const target = new URL(pgUrl());
    target.username = ROLE;
    target.password = ROLE_PASSWORD;
    const url = target.toString();

    const result = spawnSync(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", url,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--schema", namespace,
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        encoding: "utf8",
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    const text = `${result.stdout ?? ""}\n${result.stderr ?? ""}`
      .replace(/^WARNING.*$/gm, "")
      .trim();

    assert.equal(result.status, 1, `the migration must be refused; ${text}`);
    // The refusal names the table. That word is also the username, and the
    // substring redactor was consuming it.
    assert.match(
      text,
      new RegExp(`${TABLE}\\.tid is generic text`),
      `the diagnostic must keep the table it names; got: ${text}`,
    );
    assert.doesNotMatch(
      text,
      /<redacted user>\.tid/,
      `the username rule must not consume a bare word; got: ${text}`,
    );

    // The other direction, on the same message: narrowing the username rule must
    // not let the password through.
    assert.equal(
      text.includes(ROLE_PASSWORD),
      false,
      `the password must still be absent; got: ${text}`,
    );
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    if (roleCreated) {
      // The role owns whatever the deploy created, and PostgreSQL refuses to drop
      // a role while anything depends on it. Without this the NEXT run finds a
      // stale role, fails to recreate it, and SKIPS -- which is how this test
      // would quietly stop running.
      await admin.query(`DROP OWNED BY "${ROLE}" CASCADE`).catch(() => {});
      await admin.query(`DROP ROLE IF EXISTS "${ROLE}"`).catch(() => {});
    }
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

/** Narrowing the bare-word rule must not stop redacting the username where the
 *  text DOES mark it as a credential.
 *
 *  A failed authentication is among the likeliest errors an operator sees, and
 *  PostgreSQL spells it `password authentication failed for user "name"` -- the
 *  username in quotes after the word `user`, which is neither `user@` nor `user=`.
 *  The first version of this fix covered only the URL-authority and libpq-keyword
 *  forms, so it silently STOPPED redacting the username in exactly that message.
 *  Nothing caught it: the sibling suite asserts the password never leaks, and the
 *  password does not appear there. */
test("the username is still redacted where the text marks it as one", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  // Deliberately a user that does not exist: the server rejects the credentials
  // and names the user back, without needing any role to be provisioned.
  const GHOST = "zm_ghost_user";
  const GHOST_PASSWORD = "WrongPass123";
  const target = new URL(pgUrl());
  target.username = GHOST;
  target.password = GHOST_PASSWORD;

  const work = project();
  try {
    const result = spawnSync(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "status",
        "--dir", join(work, "migrations"),
        "--database-url", target.toString(),
        "--policy", join(work, "policy.toml"),
      ],
      {
        cwd: work,
        encoding: "utf8",
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    const text = `${result.stdout ?? ""}\n${result.stderr ?? ""}`
      .replace(/^WARNING.*$/gm, "")
      .trim();

    assert.match(
      text,
      /authentication failed/i,
      `the arm must reach a real authentication rejection; got: ${text}`,
    );
    assert.equal(
      text.includes(GHOST),
      false,
      `\`for user "…"\` is a credential-shaped position, so the username must ` +
        `still be redacted there; got: ${text}`,
    );
    assert.equal(
      text.includes(GHOST_PASSWORD),
      false,
      `and the password must never appear; got: ${text}`,
    );
  } finally {
    // No role and no schema here: the connection never authenticates, so this arm
    // provisions nothing to clean up.
    rmSync(work, { recursive: true, force: true });
  }
});
