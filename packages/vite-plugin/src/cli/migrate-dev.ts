#!/usr/bin/env node
// packages/vite-plugin/src/cli/migrate-dev.ts
//
// `zeroship-dev-migrate` — apply the committed migrations to the dev database.
//
// This is a SEPARATE step from `pnpm dev`, on purpose. Starting a dev server and
// migrating a database are different actions with different blast radii, and
// folding the second into the first means every restart, every file-watch
// reload, and every `pnpm dev` typo is also a schema write. Keeping them apart
// mirrors the platform (`migrated` applies at deploy; the worker only reads) and
// makes the failure legible: when the schema is wrong you re-run ONE command and
// read ONE output, instead of reading a dev-server log for a line that scrolled
// past.
//
//     pnpm migrate     # apply schema  (this)
//     pnpm dev         # serve         (reports, never applies)
//
// Exit codes: 0 applied/up-to-date, 1 failed. The non-zero exit is what makes it
// usable in CI and in `pnpm migrate && pnpm dev`.

import { existsSync } from "node:fs";
import { resolve } from "node:path";

import { genTypesFromMigrations } from "../gen-types/index.js";
import { applyMigrationsToDevSqlite, devSqliteAppPath } from "../gen-types/dev-apply.js";
import {
  collectionNamesFrom,
  readGeneratedRuntimeDescriptorAt,
} from "../gen-types/read-descriptor.js";
import { resolveDevDatabase } from "../dev-db.js";
import {
  logDatabaseUrlSource,
  parseDotenvVars,
  resolveDatabaseUrl,
} from "../dev-database-url.js";
import { readProjectConfig, selectDatabase } from "../project-config/index.js";

interface Argv {
  root: string;
  migrationsDir: string;
  outDir: string;
  /** The database's LOCAL label, and whether it is the app's `env.db`. Both
   *  come from the file; `env.db.ts` keys `EnvDatabases` on the label. */
  label: string;
  primary: boolean;
  /** The database's declared `dbs_` id. It names the file this applies into,
   *  and the dev runtime attaches that same file for the same id. */
  databaseId: string;
  /** The zeroship.jsonc that supplied them. Never null: the database has to
   *  be declared somewhere, and `databases` lives only in that file. */
  configPath: string;
}

function parseArgv(argv: string[]): Argv {
  let root = process.cwd();
  let dir: string | undefined;
  let out: string | undefined;
  let app: string | undefined;
  let database: string | undefined;

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const eq = arg.indexOf("=");
    const [flag, inlineValue] = eq === -1 ? [arg, undefined] : [arg.slice(0, eq), arg.slice(eq + 1)];
    const value = () => inlineValue ?? argv[++i];

    switch (flag) {
      case "--root":
        root = resolve(value() ?? root);
        break;
      case "--migrations":
        dir = value();
        break;
      case "--out":
        out = value();
        break;
      case "--app":
        app = value();
        break;
      case "--database":
        database = value();
        break;
      case "-h":
      case "--help":
        console.log(
          "zeroship-dev-migrate - apply committed migrations to the dev database\n\n" +
            "Usage: zeroship-dev-migrate [--root <dir>] [--app <label>] [--database <label>]\n" +
            "                            [--migrations <dir>] [--out <dir>]\n\n" +
            "The paths default to the selected database in the app's zeroship.jsonc -\n" +
            "its primary unless --database names another; the dir flags are overrides.\n\n" +
            "Run this BEFORE `pnpm dev`. The dev server reports schema state but never applies it."
        );
        process.exit(0);
    }
  }

  // THE DEFAULTS COME FROM THE FILE, not from two constants written here.
  // This binary was the FOURTH independent derivation of migrations.dir and
  // migrations.out: the build had one, the dev server had one,
  // the Rust CLI had one, and this had a fourth pair hand-typed into
  // `parseArgv`. The flags survive as overrides; only the fallback moved.
  const { config, path: configPath } = readProjectConfig(root);
  const selected = selectDatabase(config, { app, database });
  // THE DIR FLAGS OVERRIDE PATHS, NOT IDENTITY. This command regenerates
  // `env.db.ts`, which declares the database's entry on `EnvDatabases` under
  // its LABEL and declares `Env.db` only when it is the app's PRIMARY. Neither
  // fact is recoverable from a pair of directories, so an invocation with
  // nothing declaring the database says so rather than inventing a label.
  if (selected == null || configPath == null) {
    throw new Error(
      "[zeroship] no database to migrate. --migrations/--out override where the schema is " +
        "read and written, not WHICH database it is: the regenerated env.db.ts names the " +
        "database's label and whether it is the app's primary. Declare it under " +
        "`databases` and name that label in the app's `databases`.",
    );
  }
  const resolveMember = (override: string | undefined, member: "migrations" | "out") =>
    resolve(root, override ?? selected[member]);
  return {
    root,
    migrationsDir: resolveMember(dir, "migrations"),
    outDir: resolveMember(out, "out"),
    label: selected.label,
    primary: selected.primary,
    databaseId: selected.id,
    configPath,
  };
}

async function main(): Promise<number> {
  const { root, migrationsDir, outDir, label, primary, databaseId, configPath } = parseArgv(
    process.argv.slice(2),
  );
  console.log(`[zeroship] migrations=${migrationsDir} out=${outDir} (from ${configPath})`);

  if (!existsSync(migrationsDir)) {
    console.error(`[zeroship] no migrations directory at ${migrationsDir} — nothing to apply`);
    return 1;
  }

  // 1. Regenerate the artifacts FIRST. The apply's ownership registry is keyed on
  //    the descriptor's collections, so generating and applying in one command is
  //    what keeps them from disagreeing. Unlike the dev server, a failure here is
  //    fatal: this command's whole job is the schema, so a bad migration must
  //    stop it rather than be logged past.
  await genTypesFromMigrations(migrationsDir, outDir, { label, primary, check: false });
  console.log("[zeroship] gen-types: regenerated env.db.ts + schema.runtime.json");

  const collections = collectionNamesFrom(readGeneratedRuntimeDescriptorAt(outDir));

  // 2. Resolve DATABASE_URL through the SAME helper the dev server uses, so this
  //    writes the file the worker will open.
  const { databaseUrl, source } = resolveDatabaseUrl(
    process.env,
    parseDotenvVars(root),
    resolveDevDatabase(root).databaseUrl,
  );
  logDatabaseUrlSource(source, databaseUrl);

  const appPath = devSqliteAppPath(root, databaseId, databaseUrl);
  const reply = await applyMigrationsToDevSqlite({
    root,
    migrationsDir,
    collections,
    databaseId,
    databaseUrl,
  });

  const applied = reply.applied?.length ?? 0;
  const skipped = reply.skipped?.length ?? 0;
  // Report the COUNTS, not "ok". `applied=0 skipped=0` on a fresh database means
  // nothing ran, which is a failure wearing a success's clothes.
  console.log(
    `[zeroship] migrations applied=${applied} skipped=${skipped} (${label}=${databaseId}) -> ${appPath}`
  );
  if (applied === 0 && skipped === 0) {
    console.error("[zeroship] nothing was applied and nothing was skipped — the schema is UNCHANGED");
    return 1;
  }
  return 0;
}

main().then(
  (code) => process.exit(code),
  (error: unknown) => {
    console.error(`[zeroship] migrate FAILED: ${(error as Error).message}`);
    process.exit(1);
  }
);
