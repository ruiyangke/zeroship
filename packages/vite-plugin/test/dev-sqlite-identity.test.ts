import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { devDatabaseAlias, devSqliteAppPath } from "../src/gen-types/dev-apply.js";
import { readProjectConfig, selectDatabase } from "../src/project-config/index.js";

/**
 * `examples/db-todos`, a committed migration-first project. Read through the
 * plugin's own reader rather than parsed here: the id under test is the one
 * `zeroship-dev-migrate` dereferences, and a hand-built config would agree
 * with itself and with nothing else.
 */
const DB_TODOS = fileURLToPath(new URL("../../../examples/db-todos", import.meta.url));

test("the dev apply targets the file named by the DECLARED database id", () => {
  const { config, path } = readProjectConfig(DB_TODOS);
  assert.notEqual(path, null, `${DB_TODOS} must hold a zeroship.jsonc`);
  const database = selectDatabase(config);
  assert.notEqual(database, undefined, "db-todos must declare a primary database");
  assert.match(database!.id, /^dbs_[0-9a-z]{25}$/, "the declared id must be a dbs_ id");

  // The alias the SQLite backend attaches under is the binding's SCHEMA, and
  // `schema_name` (crates/zeroship-core/src/database_derivation.rs) renders a
  // database id as `db_<id>`. The file is that alias.
  assert.equal(devDatabaseAlias(database!.id), `db_${database!.id}`);
  assert.equal(
    devSqliteAppPath(DB_TODOS, database!.id),
    `${DB_TODOS}/.zeroship/zs-db_${database!.id}.sqlite`,
  );
});

test("two declared databases are two files", () => {
  // The control for the arm above: the path is a function OF the id, so a
  // composer that ignored its argument and returned a constant would pass
  // every assertion there and none here.
  const first = devSqliteAppPath("/project", "dbs_03evr3oqx1200cfkwyailh8l8");
  const second = devSqliteAppPath("/project", "dbs_03evr3oqx1200mf301taes352");
  assert.notEqual(first, second);
});

test("a declared database is ONE file", () => {
  // The journal describes this database and lives inside it, so there is no
  // second path to name. Nothing beside the database file carries the
  // `.migrations` spelling the sidecar used, and this is where a reintroduced
  // one would be caught: a composer that grew a second field again would have
  // to grow a second CALLER, and the apply's driver takes one path.
  const path = devSqliteAppPath("/project", "dbs_03evr3oqx1200cfkwyailh8l8");
  assert.equal(typeof path, "string");
  assert.equal(path.endsWith(".migrations.sqlite"), false);
});
