// `zero-migrate` — the command-line surface for the host runtime.
//
// A THIN arg-parser over the host verbs this package already exports
// (`apply`/`plan`/`status`/`validate`) plus pure-JS `new` scaffolding — the engine
// facade does the real work over the native addon and the `pg`/`mysql2` driver seam.
//
// Commands:
//   new <name>          Scaffold a fresh `<14-digit-ts>_<name>.ts` op-DSL migration
//                       (imports `{ table, t } from "zero-migrate"`). OFFLINE.
//   plan   [dir]        DB-free load + structural/confinement/ownership VERIFY of every
//                       migration in `dir` (the fast pre-apply gate). OFFLINE.
//   preview [dir]       DB-free: print the authored `{ ir_version, name, ops }`
//                       envelope for each migration (the op-IR the addon would lower).
//                       OFFLINE.
//   apply  [dir]        Apply every migration in `dir` over the `--database-url`
//                       driver (`pg`/`mysql2` seam) in filename order.
//   status [dir]        Reconcile against the live journal over the `--database-url`
//                       driver.
//
// Migration discovery: `*.{ts,mts,cts,js,mjs,cjs}` under `dir` (default `./migrations`),
// excluding `.d.ts`, sorted by filename (the migration order contract). Each is
// dynamically `import()`ed; the module must export `up()` (or `default.up`). Plain
// Node cannot import `.ts` — run the CLI under a `.ts` loader (e.g. `tsx`/`bun`) or
// point it at pre-built `.js`/`.mjs`.

import { readdir, mkdir, writeFile, access, readFile } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { join, resolve, isAbsolute } from "node:path";
import { pathToFileURL } from "node:url";
import {
  apply,
  history,
  plan,
  resolvePending,
  status,
  currentIrVersion,
  type DriverConfig,
} from "./index.js";
import {
  buildEnvelope,
  deriveNameFromPath,
  resolveMigrationName,
  type MigrationModule,
} from "zero-migrate/internal/recorder";

/** The default migration directory (dbmate/Flyway convention). */
const DEFAULT_DIR = "./migrations";
/** The default confined project schema the lower pins ops to. */
const DEFAULT_SCHEMA = "public";
/** The default deploying app id when none is given. */
const DEFAULT_OWNER_APP = "app_cli";

/** This package's version, read from its own `package.json`. `new URL("../package.json",
 *  import.meta.url)` resolves to the package root whether the CLI runs from published
 *  `dist/` or a bundled dev chunk. */
function packageVersion(): string {
  try {
    const pkg = readFileSync(new URL("../package.json", import.meta.url), "utf8");
    return (JSON.parse(pkg) as { version?: string }).version ?? "0.0.0";
  } catch {
    return "0.0.0";
  }
}

/** Lazily activate a TypeScript loader so `.ts`/`.mts`/`.cts` migrations can be
 *  dynamically `import()`ed under plain Node. No-op when the set is pure `.js`, when a
 *  parent loader (tsx / ts-node via `--import` or `NODE_OPTIONS`) already handles TS, or
 *  on Bun (native TS). Registers `tsx` once if resolvable; otherwise fails with
 *  actionable guidance. */
let tsLoaderReady = false;
async function ensureTsLoader(files: readonly MigrationFile[]): Promise<void> {
  if (tsLoaderReady) return;
  const needsTs = files.some((f) => /\.(ts|mts|cts)$/i.test(f.path));
  if (!needsTs) {
    tsLoaderReady = true;
    return;
  }
  const parentLoaderActive =
    process.versions.bun !== undefined ||
    process.execArgv.some((a) => a.includes("tsx") || a.includes("ts-node")) ||
    (process.env.NODE_OPTIONS ?? "").includes("tsx");
  if (parentLoaderActive) {
    tsLoaderReady = true;
    return;
  }
  try {
    const tsx = (await import("tsx/esm/api")) as { register: () => unknown };
    tsx.register();
    tsLoaderReady = true;
  } catch {
    throw new CliError(
      "these migrations are TypeScript (.ts) but no TypeScript loader is active. Install " +
        "`tsx` (it ships as an optional dependency of zero-migrate-cli) so the CLI can load " +
        "them, run the CLI under `npx tsx zero-migrate ...` or Bun, or point --dir at compiled " +
        ".js migrations.",
    );
  }
}

/** A migration source file + its dynamic-import URL. */
interface MigrationFile {
  path: string;
  /** Filename-derived label (the recorder's `nameFallback`). */
  label: string;
}

/** An imported migration paired with its filename-derived fallback identity. */
interface LoadedMigration {
  file: MigrationFile;
  migration: MigrationModule;
}

/** The parsed CLI invocation. */
interface Args {
  command: string;
  /** The positional after the command (a `dir` for most verbs, a `name` for `new`). */
  positional?: string;
  dir: string;
  databaseUrl?: string;
  /** Dialect selected for offline plan validation. Defaults to PostgreSQL. */
  dialect?: "postgres" | "mysql" | "sqlite";
  /** Path to the trusted JSON table-ownership registry. */
  registryPath?: string;
  ownerApp: string;
  projectSchema: string;
  /** `--json` — machine-readable output where a verb supports it. */
  json: boolean;
  /** `--approve` grants operator approval for reviewed destructive/data-rewrite steps. */
  approved: boolean;
  /** Resolve the pending rename by keeping the new column. */
  resolveApply: boolean;
  /** Resolve the pending rename by keeping the old column. */
  resolveAbort: boolean;
}

/** Parse value-taking flags, valueless boolean flags, and positionals. Unknown
 * flags and inline values on valueless flags error. */
function parseArgs(argv: string[]): Args {
  const args: Args = {
    command: "",
    dir: DEFAULT_DIR,
    ownerApp: process.env.ZERO_MIGRATE_OWNER_APP ?? DEFAULT_OWNER_APP,
    projectSchema: process.env.ZERO_MIGRATE_SCHEMA ?? DEFAULT_SCHEMA,
    json: false,
    approved: false,
    resolveApply: false,
    resolveAbort: false,
  };
  let helpRequested = false;
  let versionRequested = false;
  const positionals: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const tok = argv[i];
    if (!tok.startsWith("--")) {
      positionals.push(tok);
      continue;
    }
    const eq = tok.indexOf("=");
    const key = eq === -1 ? tok.slice(2) : tok.slice(2, eq);
    const inlineVal = eq === -1 ? undefined : tok.slice(eq + 1);
    const takeVal = (): string => {
      if (inlineVal !== undefined) return inlineVal;
      const next = argv[++i];
      if (next === undefined) throw new CliError(`flag --${key} needs a value`);
      return next;
    };
    const rejectInlineVal = (): void => {
      if (inlineVal !== undefined) {
        throw new CliError(`flag --${key} does not take a value`);
      }
    };
    switch (key) {
      case "dir":
        args.dir = takeVal();
        break;
      case "database-url":
        args.databaseUrl = takeVal();
        break;
      case "dialect": {
        const value = takeVal();
        if (value !== "postgres" && value !== "mysql" && value !== "sqlite") {
          throw new CliError(
            `flag --dialect must be postgres, mysql, or sqlite; got ${JSON.stringify(value)}`,
          );
        }
        args.dialect = value;
        break;
      }
      case "registry":
        args.registryPath = takeVal();
        break;
      case "owner-app":
        args.ownerApp = takeVal();
        break;
      case "schema":
        args.projectSchema = takeVal();
        break;
      case "json":
        rejectInlineVal();
        args.json = true;
        break;
      case "approve":
        rejectInlineVal();
        args.approved = true;
        break;
      case "apply":
        rejectInlineVal();
        args.resolveApply = true;
        break;
      case "abort":
        rejectInlineVal();
        args.resolveAbort = true;
        break;
      case "version":
        rejectInlineVal();
        versionRequested = true;
        break;
      case "help":
        rejectInlineVal();
        helpRequested = true;
        break;
      default:
        throw new CliError(`unknown flag --${key}`);
    }
  }
  if (versionRequested) {
    args.command = "version";
    return args;
  }
  if (helpRequested) {
    args.command = "help";
    return args;
  }
  if (!args.command) args.command = positionals.shift() ?? "";
  args.positional = positionals.shift();
  if (positionals.length > 0) {
    throw new CliError(`unexpected positional argument ${JSON.stringify(positionals[0])}`);
  }
  if (
    args.command !== "new" &&
    args.command !== "resolve-pending" &&
    args.positional !== undefined
  ) {
    throw new CliError(
      `command ${JSON.stringify(args.command)} does not accept positional arguments; use --dir`,
    );
  }
  if (args.dialect !== undefined && args.command !== "plan") {
    throw new CliError("flag --dialect is only valid with the plan command");
  }
  if ((args.resolveApply || args.resolveAbort) && args.command !== "resolve-pending") {
    throw new CliError("flags --apply and --abort are only valid with resolve-pending");
  }
  if (
    args.registryPath !== undefined &&
    args.command !== "plan" &&
    args.command !== "apply" &&
    args.command !== "status"
  ) {
    throw new CliError("flag --registry is only valid with plan, apply, or status");
  }
  // Fall back only when the flag was absent. An explicitly empty flag is an error.
  if (args.databaseUrl === undefined) {
    const env = process.env.DATABASE_URL;
    if (env && env.length > 0) args.databaseUrl = env;
  }
  return args;
}

/** A CLI-level failure with a clean message (mapped to a non-zero exit, no stack). */
class CliError extends Error {}

/** Read and validate an authoritative table-to-owner registry from JSON. */
async function loadRegistry(path: string | undefined): Promise<Record<string, string>> {
  if (path === undefined) return {};
  if (path.length === 0) throw new CliError("flag --registry needs a non-empty file path");

  let source: string;
  try {
    source = await readFile(path, "utf8");
  } catch (error) {
    throw new CliError(`read registry file ${path}: ${(error as Error).message}`);
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(source);
  } catch (error) {
    throw new CliError(
      `registry file ${path} must contain valid JSON: ${(error as Error).message}`,
    );
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new CliError(
      `registry file ${path} must contain a JSON object mapping table names to owner app IDs`,
    );
  }

  const registry: Record<string, string> = {};
  for (const [table, owner] of Object.entries(parsed)) {
    if (table.length === 0 || typeof owner !== "string" || owner.length === 0) {
      throw new CliError(
        `registry file ${path} must map each non-empty table name to a non-empty owner app ID`,
      );
    }
    Object.defineProperty(registry, table, {
      value: owner,
      enumerable: true,
      configurable: true,
      writable: true,
    });
  }
  return registry;
}

/** Select the supported Node driver from a database URL scheme. */
function driverFor(databaseUrl: string): DriverConfig {
  const lower = databaseUrl.trimStart().toLowerCase();
  if (lower.startsWith("postgres://") || lower.startsWith("postgresql://")) {
    return { kind: "postgres", url: databaseUrl };
  }
  if (lower.startsWith("mysql://")) {
    return { kind: "mysql", url: databaseUrl };
  }
  if (lower.startsWith("sqlite:") || lower.endsWith(".sqlite") || lower.endsWith(".db")) {
    throw new CliError(
      "SQLite is not supported by the Node CLI. Use the Rust API for SQLite, or " +
        "provide a postgres:// or mysql:// database URL.",
    );
  }
  throw new CliError(
    "could not infer a driver from the database URL (expected a postgres:// or mysql:// scheme)",
  );
}

/** Discover migration source files under `dir`, sorted by filename (order contract). */
async function discover(dir: string): Promise<MigrationFile[]> {
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch (e) {
    throw new CliError(`read migrations dir ${dir}: ${(e as Error).message}`);
  }
  const files = entries
    .filter((n) => /\.(ts|mts|cts|js|mjs|cjs)$/i.test(n) && !/\.d\.ts$/i.test(n))
    .sort();
  return files.map((n) => {
    const path = resolve(dir, n);
    return { path, label: deriveNameFromPath(n) };
  });
}

/** Dynamic-import a migration module by absolute path. */
async function importMigration(path: string): Promise<MigrationModule> {
  const url = isAbsolute(path) ? pathToFileURL(path).href : path;
  return (await import(url)) as MigrationModule;
}

/** Import the complete ordered migration set without executing any `up()` body. */
async function importMigrations(files: readonly MigrationFile[]): Promise<LoadedMigration[]> {
  const loaded: LoadedMigration[] = [];
  for (const file of files) {
    loaded.push({ file, migration: await importMigration(file.path) });
  }
  return loaded;
}

/** Reject ambiguous plan identities before planning or opening a database session. */
function assertUniqueMigrationNames(migrations: readonly LoadedMigration[]): void {
  const firstFileByName = new Map<string, MigrationFile>();
  for (const { file, migration } of migrations) {
    const name = resolveMigrationName(migration, file.label);
    const first = firstFileByName.get(name);
    if (first !== undefined) {
      throw new CliError(
        `duplicate migration name ${JSON.stringify(name)}: ` +
          `${first.label} and ${file.label} resolve to the same plan identity; ` +
          "export a unique name from each migration",
      );
    }
    firstFileByName.set(name, file);
  }
}

/** `new <name>` — scaffold a fresh op-DSL migration. Validates the name, refuses to
 *  clobber, and prints the created path. The scaffold imports `{ table, t }` from the
 *  `zero-migrate` DSL package (the current fluent surface). */
async function runNew(args: Args): Promise<number> {
  const name = args.positional;
  if (!name) throw new CliError("`new` needs a migration name: zero-migrate new <name>");
  if (!/^[A-Za-z0-9_]+$/.test(name)) {
    const suggested = name.replace(/[^A-Za-z0-9_]+/g, "_").replace(/^_+|_+$/g, "");
    const hint = suggested ? ` (did you mean \`${suggested}\`?)` : "";
    throw new CliError(
      `invalid migration name ${JSON.stringify(name)}: allowed characters are ` +
        `[A-Za-z0-9_] (use \`_\` for spaces/dashes)${hint}`,
    );
  }
  const stamp = timestamp14(new Date());
  const filename = `${stamp}_${name}.ts`;
  const path = resolve(args.dir, filename);
  try {
    await access(path);
    throw new CliError(`refusing to overwrite existing file: ${path}`);
  } catch (e) {
    if (e instanceof CliError) throw e;
    // ENOENT — the file does not exist, which is what we want.
  }
  await mkdir(args.dir, { recursive: true });
  await writeFile(path, scaffold(name), "utf8");
  process.stdout.write(`Creating migration: ${path}\n`);
  return 0;
}

/** The op-DSL migration scaffold body — emits a `zero-migrate` DSL module. */
function scaffold(name: string): string {
  return `import { table, t } from "zero-migrate";

export const name = ${JSON.stringify(name)};

export default {
  up() {
    // Author your schema change with the fluent op DSL, e.g.:
    // table("widgets").create({
    //   columns: {
    //     label: t.text().notNull(),
    //   },
    // });
  },
};
`;
}

/** Format a 14-digit \`YYYYMMDDHHMMSS\` UTC timestamp (the migration-file ordering key). */
function timestamp14(now: Date): string {
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return (
    p(now.getUTCFullYear(), 4) +
    p(now.getUTCMonth() + 1) +
    p(now.getUTCDate()) +
    p(now.getUTCHours()) +
    p(now.getUTCMinutes()) +
    p(now.getUTCSeconds())
  );
}

/** One `plan` verdict line. */
interface PlanLine {
  label: string;
  ok: boolean;
  opCount: number;
  irVersion?: number;
  error?: string;
}

/** `preview [dir]` — DB-free: print the authored op-IR envelope for each migration. */
async function runPreview(args: Args): Promise<number> {
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const irVersion = currentIrVersion();
  const envelopes = [];
  for (const f of files) {
    const mod = await importMigration(f.path);
    envelopes.push(buildEnvelope(mod, { irVersion, nameFallback: f.label }));
  }
  if (args.json) {
    process.stdout.write(JSON.stringify(envelopes, null, 2) + "\n");
  } else {
    for (const env of envelopes) {
      process.stdout.write(
        `preview ${env.name}: ir_version=${env.ir_version} ops=${env.ops.length}\n`,
      );
      process.stdout.write(JSON.stringify(env.ops, null, 2) + "\n");
    }
  }
  return 0;
}

/** `apply [dir]` — apply every migration over the `--database-url` driver in order. */
async function runApply(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError(
      "missing database URL (pass --database-url or set DATABASE_URL)",
    );
  }
  const driver = driverFor(args.databaseUrl);
  const registry = await loadRegistry(args.registryPath);
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const migrations = await importMigrations(files);
  assertUniqueMigrationNames(migrations);
  for (const { file, migration } of migrations) {
    const outcome = await apply({
      migration,
      ownerApp: args.ownerApp,
      projectSchema: args.projectSchema,
      driver,
      registry,
      nameFallback: file.label,
      approved: args.approved,
    });
    process.stdout.write(`apply ${file.label}: ${JSON.stringify(outcome)}\n`);
  }
  return 0;
}

/** `status [dir]` — reconcile against the live journal over the `--database-url`
 *  driver. (Reads the journal; the migration set is discovered for the pending view.) */
async function runStatus(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError(
      "missing database URL (pass --database-url or set DATABASE_URL)",
    );
  }
  const driver = driverFor(args.databaseUrl);
  const registry = await loadRegistry(args.registryPath);
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const migrations = await Promise.all(files.map((file) => importMigration(file.path)));
  const reply = await status({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    driver,
    registry,
    migrations,
    nameFallbacks: files.map((file) => file.label),
  });
  if (args.json) {
    process.stdout.write(JSON.stringify(reply, null, 2) + "\n");
  } else {
    process.stdout.write(`status: ${JSON.stringify(reply)}\n`);
  }
  return 0;
}

/** Complete or abort one outstanding PostgreSQL online rename. */
async function runResolvePending(args: Args): Promise<number> {
  const pendingVersion = args.positional;
  if (!pendingVersion) {
    throw new CliError(
      "`resolve-pending` needs a pending version: zero-migrate resolve-pending <pending-version>",
    );
  }
  if (args.resolveApply === args.resolveAbort) {
    throw new CliError("choose exactly one of --apply or --abort");
  }
  if (!args.approved) {
    throw new CliError("resolve-pending requires --approve after reviewing the column drop");
  }
  if (!args.databaseUrl) {
    throw new CliError("missing database URL (pass --database-url or set DATABASE_URL)");
  }
  const driver = driverFor(args.databaseUrl);
  if (driver.kind !== "postgres") {
    throw new CliError("resolve-pending supports only PostgreSQL online renames");
  }
  const outcome = await resolvePending({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    pendingVersion,
    action: args.resolveApply ? "apply" : "abort",
    driver,
    approved: true,
    appliedBy: "cli",
  });
  process.stdout.write(`resolve-pending ${pendingVersion}: ${JSON.stringify(outcome)}\n`);
  return 0;
}

/** `history` prints the append-only migration audit trail over the
 *  `--database-url` driver. PostgreSQL only (the journal history verb is PG-backed). */
async function runHistory(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError("missing database URL (pass --database-url or set DATABASE_URL)");
  }
  const driver = driverFor(args.databaseUrl);
  if (driver.kind !== "postgres") {
    throw new CliError("history supports only PostgreSQL");
  }
  const reply = await history({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    driver,
  });
  if (args.json) {
    process.stdout.write(
      JSON.stringify(reply, (_k, v) => (typeof v === "bigint" ? v.toString() : v), 2) + "\n",
    );
  } else {
    for (const e of reply.events) {
      process.stdout.write(
        `#${e.eventSeq} ${e.kind} ${e.name} (${e.version}) by ${e.appliedBy} at ${e.at}\n`,
      );
    }
    if (reply.events.length === 0) process.stdout.write("history: (no recorded events)\n");
  }
  return 0;
}

/** The `--help` / usage text. */
const USAGE = `zero-migrate: database migrations from JavaScript

Usage:
  zero-migrate new <name> [--dir <dir>]
  zero-migrate plan    [--dir <dir>] [--dialect <name>] [--registry <file>] [--owner-app <app>] [--schema <schema>] [--json]
  zero-migrate preview [--dir <dir>] [--json]
  zero-migrate apply   [--dir <dir>] --database-url <url> [--registry <file>] [--owner-app <app>] [--schema <schema>] [--approve]
  zero-migrate status  [--dir <dir>] --database-url <url> [--registry <file>] [--owner-app <app>] [--schema <schema>] [--json]
  zero-migrate history [--database-url <url>] [--owner-app <app>] [--schema <schema>] [--json]
  zero-migrate resolve-pending <pending-version> (--apply | --abort) --approve --database-url <url> [--owner-app <app>] [--schema <schema>]
  zero-migrate --version

Flags:
  --dir <dir>           Migration directory (default ./migrations)
  --database-url <url>  postgres:// or mysql:// DSN (or the DATABASE_URL env)
  --dialect <name>      plan dialect: postgres, mysql, or sqlite (default postgres)
  --registry <file>     Trusted JSON map of table names to owner app IDs
  --owner-app <app>     Deploying app id stamped as owner_app (default app_cli)
  --schema <schema>     Confined project schema (default public)
  --approve             Approve reviewed destructive changes and backfills
  --apply               Complete an online rename and keep the new column
  --abort               Abort an online rename and keep the old column
  --json                Machine-readable output where supported
  --version             Print the zero-migrate version
  --help                This help

new/plan/preview are offline and do not connect to a database. apply/status/history
support PostgreSQL (history is PostgreSQL only); apply/status also support MySQL 8.
Use the Rust API to apply SQLite migrations. TypeScript (.ts) migrations load via
tsx (an optional dependency) -- install it, or run under "npx tsx" or Bun.
`;

/** Entry point: parse, dispatch, map thrown `CliError` to a clean non-zero exit. */
export async function main(argv: string[]): Promise<number> {
  let args: Args;
  try {
    args = parseArgs(argv);
  } catch (e) {
    process.stderr.write(`zero-migrate: ${(e as Error).message}\n`);
    return 1;
  }
  if (args.command === "version") {
    process.stdout.write(`${packageVersion()}\n`);
    return 0;
  }
  if (args.command === "" || args.command === "help") {
    process.stdout.write(USAGE);
    return args.command === "" ? 1 : 0;
  }
  try {
    switch (args.command) {
      case "new":
        return await runNew(args);
      case "plan":
        return await runPlanResolved(args);
      case "preview":
        return await runPreview(args);
      case "apply":
        return await runApply(args);
      case "status":
        return await runStatus(args);
      case "history":
        return await runHistory(args);
      case "resolve-pending":
        return await runResolvePending(args);
      default:
        process.stderr.write(`zero-migrate: unknown command ${JSON.stringify(args.command)}\n`);
        process.stdout.write(USAGE);
        return 1;
    }
  } catch (e) {
    process.stderr.write(`zero-migrate: ${(e as Error).message}\n`);
    return 1;
  }
}

/** `plan` with the async import resolved (the discover→import→validate loop). */
async function runPlanResolved(args: Args): Promise<number> {
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const registry = await loadRegistry(args.registryPath);
  const migrations = await importMigrations(files);
  assertUniqueMigrationNames(migrations);
  const reports: PlanLine[] = [];
  for (const { file, migration } of migrations) {
    const report = plan({
      migration,
      ownerApp: args.ownerApp,
      projectSchema: args.projectSchema,
      dialect: args.dialect ?? "postgres",
      registry,
      nameFallback: file.label,
    });
    reports.push({
      label: report.envelope.name || file.label,
      ok: report.ok,
      opCount: report.op_count ?? report.envelope.ops.length,
      irVersion: report.ir_version,
      error: report.error,
    });
  }
  if (args.json) {
    process.stdout.write(JSON.stringify(reports, null, 2) + "\n");
  } else {
    for (const r of reports) {
      const head = r.ok ? "ok" : "ERROR";
      process.stdout.write(`plan ${r.label}: ${head} (${r.opCount} ops)\n`);
      if (r.error) process.stdout.write(`  ${r.error}\n`);
    }
  }
  return reports.every((r) => r.ok) ? 0 : 1;
}
