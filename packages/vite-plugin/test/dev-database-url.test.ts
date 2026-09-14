import test from "node:test";
import assert from "node:assert/strict";

import { resolveDatabaseUrl, DevDatabaseUrlSchemeError } from "../src/dev-database-url.js";
import { devSqliteDir } from "../src/gen-types/dev-apply.js";

/**
 * `resolveDatabaseUrl` is the ONE resolution shared by `migrate-dev.ts` and
 * `dev-server.ts` (both consumers import it rather than re-deriving the
 * precedence). Today it returns whatever it finds with no branch on the
 * scheme, so a Postgres `DATABASE_URL` is silently routed to the SQLite dev
 * apply path and the runtime child at the same time — see
 * `docs/proposals/2026-08-26-sc4-dev-and-hmr-mechanism.md`, Decision 1.
 *
 * These tests pin the fix: a non-SQLite dev URL is refused AT RESOLUTION,
 * before any caller can derive a path or apply a migration from it.
 */

const SQLITE_DEFAULT = "sqlite:.zeroship/dev.sqlite";

test("memory and empty SQLite selectors are refused before runtime or migration setup", () => {
  for (const url of ["sqlite:", "sqlite://", "sqlite::memory:", "sqlite://:MEMORY:",
    "sqlite:file:db?mode=memory", "sqlite:db?mode=memory&cache=shared"]) {
    assert.throws(() => resolveDatabaseUrl({ DATABASE_URL: url }, {}, SQLITE_DEFAULT), /must name a SQLite file/);
    assert.throws(() => devSqliteDir("/project", url), /must name a SQLite file/);
  }
});

test("runtime and migration setup resolve the same explicit database file", () => {
  assert.equal(devSqliteDir("/project", "sqlite:.state/dev.sqlite"), "/project/.state");
  assert.equal(devSqliteDir("/project", "sqlite:///tmp/dev.sqlite"), "/tmp");
  assert.equal(devSqliteDir("/project", "sqlite://state/dev.sqlite"), "/project/state");
});

test("a postgres:// DATABASE_URL from the shell env is refused, naming the shell as source", () => {
  assert.throws(
    () =>
      resolveDatabaseUrl(
        { DATABASE_URL: "postgres://user:secret@db.example.com:5432/app" } as NodeJS.ProcessEnv,
        {},
        SQLITE_DEFAULT,
      ),
    (err: unknown) => {
      assert.ok(err instanceof DevDatabaseUrlSchemeError, "expected DevDatabaseUrlSchemeError");
      const message = (err as Error).message;
      assert.match(message, /DATABASE_URL/, "message must name the variable");
      assert.match(message, /shell/i, "message must name the shell as the source");
      assert.match(message, /postgres/i, "message must name the offending scheme");
      // The URL carries a credential — the message must never repeat it verbatim.
      assert.doesNotMatch(message, /secret/, "message must not leak the raw URL/credentials");
      return true;
    },
  );
});

test("a postgresql:// DATABASE_URL from .env is refused, naming .env as the source", () => {
  assert.throws(
    () =>
      resolveDatabaseUrl(
        {} as NodeJS.ProcessEnv,
        { DATABASE_URL: "postgresql://db.example.com/app" },
        SQLITE_DEFAULT,
      ),
    (err: unknown) => {
      assert.ok(err instanceof DevDatabaseUrlSchemeError, "expected DevDatabaseUrlSchemeError");
      const message = (err as Error).message;
      assert.match(message, /\.env/, "message must name .env as the source");
      assert.doesNotMatch(message, /\bshell\b/i, "must not misattribute the source to the shell");
      return true;
    },
  );
});

test("the SQLite dev default still resolves unchanged", () => {
  const { databaseUrl, source } = resolveDatabaseUrl({} as NodeJS.ProcessEnv, {}, SQLITE_DEFAULT);
  assert.equal(databaseUrl, SQLITE_DEFAULT);
  assert.equal(source, "default");
});

test("control: a valid sqlite: URL from the shell passes through untouched", () => {
  const customSqlite = "sqlite:/tmp/some/custom/dev.sqlite";
  const { databaseUrl, source } = resolveDatabaseUrl(
    { DATABASE_URL: customSqlite } as NodeJS.ProcessEnv,
    {},
    SQLITE_DEFAULT,
  );
  // Proves the guard rejects on SCHEME, not on "the string was non-empty" —
  // a non-default, non-empty sqlite: value must still sail through.
  assert.equal(databaseUrl, customSqlite);
  assert.equal(source, "shell");
});

test("rejects by scheme, not by substring match on the word postgres", () => {
  // The word "postgres" appears in this URL, but the SCHEME is sqlite:. A
  // substring-based guard (`url.includes("postgres")`) would wrongly reject
  // this; a scheme-based guard must not.
  const sqliteUrlNamingPostgres = "sqlite:./postgres-migration-backup/dev.sqlite";
  const { databaseUrl } = resolveDatabaseUrl(
    { DATABASE_URL: sqliteUrlNamingPostgres } as NodeJS.ProcessEnv,
    {},
    SQLITE_DEFAULT,
  );
  assert.equal(databaseUrl, sqliteUrlNamingPostgres);
});

test("a mysql:// DATABASE_URL is refused with the mysql scheme named", () => {
  assert.throws(
    () =>
      resolveDatabaseUrl(
        { DATABASE_URL: "mysql://root@localhost/app" } as NodeJS.ProcessEnv,
        {},
        SQLITE_DEFAULT,
      ),
    (err: unknown) => {
      assert.ok(err instanceof DevDatabaseUrlSchemeError);
      assert.match((err as Error).message, /mysql/i);
      return true;
    },
  );
});
