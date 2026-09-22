import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { devDatabaseAlias, devSqlitePaths } from "../src/gen-types/dev-apply.js";
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
  assert.deepEqual(devSqlitePaths(DB_TODOS, database!.id), {
    appPath: `${DB_TODOS}/.zeroship/zs-db_${database!.id}.sqlite`,
    journalPath: `${DB_TODOS}/.zeroship/zs-db_${database!.id}.migrations.sqlite`,
  });
});

test("two declared databases are two files", () => {
  // The control for the arm above: the path is a function OF the id, so a
  // composer that ignored its argument and returned a constant would pass
  // every assertion there and none here.
  const first = devSqlitePaths("/project", "dbs_03evr3oqx1200cfkwyailh8l8");
  const second = devSqlitePaths("/project", "dbs_03evr3oqx1200mf301taes352");
  assert.notEqual(first.appPath, second.appPath);
  assert.notEqual(first.journalPath, second.journalPath);
  assert.notEqual(first.appPath, first.journalPath);
});
