// `zero-migrate` — the command-line surface for the host runtime.
//
// A thin arg-parser over the host runtime plus pure-JS `new` scaffolding. The
// engine facade does the real work over the native addon and database drivers.
//
// Commands:
//   new <name>          Scaffold a fresh `<14-digit-ts>_<name>.ts` op-DSL migration
//                       (imports `{ table, t } from "zero-migrate"`). OFFLINE.
//   lint   [dir]        DB-free verification for every supported dialect. OFFLINE.
//   plan   [dir]        Reconcile live status and render pending SQL without apply.
//   apply  [dir]        Apply every migration in `dir` over the `--database-url`
//                       driver (`pg`/`mysql2` seam) in filename order.
//   status [dir]        Reconcile against the live journal over the `--database-url`
//                       driver.
//
// Migration discovery: `*.{ts,mts,cts,js,mjs,cjs}` under `dir` (default `./migrations`),
// excluding `.d.ts`, sorted by filename (the migration order contract). Each is
// dynamically `import()`ed; the module must export `schema()` for DDL, or `data()`
// with exactly one of `inverse()` / `irreversible` for DML. Plain Node cannot
// import `.ts` — run the CLI under a `.ts` loader (e.g. `tsx`/`bun`) or point it at
// pre-built `.js`/`.mjs`.

import { readdir, mkdir, writeFile, access, readFile } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { extname, join, resolve, isAbsolute } from "node:path";
import { pathToFileURL } from "node:url";
import {
  apply,
  history,
  previewSql,
  resolvePending,
  rollback,
  statusEnvelopes,
  currentIrVersion,
  type DriverConfig,
  type NetworkSecurityOptions,
  type RollbackOutcome,
} from "./index.js";
import {
  loadAddon,
  type AdvisoryDto,
  type RollbackTargetDto,
  type StatusReply,
} from "./addon.js";
import { resolveCliConfig, type CliConfigValues } from "./config.js";
import {
  buildEnvelope,
  deriveNameFromPath,
  resolveMigrationName,
  type IrEnvelope,
  type MigrationModule,
} from "zero-migrate/internal/recorder";

/** The default migration directory (dbmate/Flyway convention). */
const DEFAULT_DIR = "./migrations";
/** The default confined project schema the lower pins ops to. */
const DEFAULT_SCHEMA = "public";
/** The default deploying app id when none is given. */
const DEFAULT_OWNER_APP = "app_cli";
/** Offline fallback when lint is run without a configured charter. */
const NO_INJECT_POLICY = "policy_version = 1\n";
const ALL_DIALECTS = ["postgres", "mysql", "sqlite"] as const;
type Dialect = (typeof ALL_DIALECTS)[number];

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

/** `version --verbose`: the CLI version PLUS the identity of the addon actually
 *  loaded. `packageVersion()` names the JavaScript that is running; it says nothing
 *  about which native binary answered, and a stale prebuilt `.node` is wrong in
 *  exactly that gap while every other call still succeeds. `sourceDigest` is a
 *  sha256 over the workspace source the binary was built from, so it separates the
 *  code in the tree from the code in memory.
 *
 *  Loading is confined to this branch: the caller has opted in, so a missing or
 *  broken addon is reported here rather than being allowed to break plain `version`.
 */
function versionVerbose(asJson: boolean): number {
  const cliVersion = packageVersion();
  let info;
  try {
    info = loadAddon().buildInfo();
  } catch (e) {
    process.stderr.write(
      `zero-migrate: cannot report the addon identity: ${(e as Error).message}\n`,
    );
    return 1;
  }
  if (asJson) {
    process.stdout.write(
      `${JSON.stringify(
        {
          cliVersion,
          addon: {
            version: info.version,
            irVersion: info.irVersion,
            sourceDigest: info.sourceDigest,
          },
        },
        null,
        2,
      )}\n`,
    );
    return 0;
  }
  process.stdout.write(
    `zero-migrate ${cliVersion}\n` +
      `  addon version  ${info.version}\n` +
      `  addon ir       ${info.irVersion}\n` +
      `  addon source   ${info.sourceDigest}\n`,
  );
  return 0;
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
  /** Optional SQLite migration-journal file override. */
  journalPath?: string;
  /** `--tls-ca <file>`: path to a PEM CA bundle to PIN for the network driver. */
  tlsCaPath?: string;
  /** `--host-allowlist <csv>`: comma-separated hosts the driver may connect to. */
  hostAllowlist?: string;
  /** `--query-timeout <ms>`: per-verb query timeout for the network driver. */
  queryTimeoutMs?: string;
  /** Resolved host-enforced transport controls for the network driver. */
  security?: NetworkSecurityOptions;
  /** Dialect selected for offline lint validation. Defaults to all dialects. */
  dialect?: Dialect;
  /** Path to the trusted JSON table-ownership registry. */
  registryPath?: string;
  /** Ordered paths to table-shape policy files. The first path is the root. */
  policyPaths: string[];
  ownerApp: string;
  projectSchema: string;
  /** `--json` — machine-readable output where a verb supports it. */
  json: boolean;
  /** `version --verbose` - also report the loaded addon's identity. Its own flag
   *  rather than a change to `version`, because `$(zero-migrate version)` captures
   *  all of stdout and `--json` is already accepted (and ignored) by `version`. */
  verbose: boolean;
  /** `--approve` grants operator approval for reviewed destructive/data-rewrite steps. */
  approved: boolean;
  /** `lint --explain` renders SQL for every selected dialect. */
  explain: boolean;
  /** `status --strict` fails on pending or dirty state. */
  strict: boolean;
  /** Resolve the pending rename by keeping the new column. */
  resolveCommit: boolean;
  /** Resolve the pending rename by keeping the old column. */
  resolveRollback: boolean;
  /** `rollback --to <version>`: unwind everything applied after this version. */
  rollbackTo?: string;
  /** `rollback --steps <n>`: unwind the n most recently applied migrations. */
  rollbackSteps?: string;
  /** `rollback --all`: unwind every applied migration. */
  rollbackAll: boolean;
  /** `rollback --force`: cross a migration that declares no down by skipping it. */
  force: boolean;
  /** `rollback --backup-acknowledged`: the operator states a backup exists. */
  backupAcknowledged: boolean;
  /** Explicit config file and environment selectors. */
  configPath?: string;
  environment?: string;
  /** True only when the URL came from `--database-url`. */
  databaseUrlFromFlag: boolean;
  /** Values whose flags were actually present, for config precedence. */
  explicitConfig: CliConfigValues;
}

/** Parse value-taking flags, valueless boolean flags, and positionals. Unknown
 * flags and inline values on valueless flags error. */
function parseArgs(argv: string[]): Args {
  const args: Args = {
    command: "",
    dir: DEFAULT_DIR,
    ownerApp: DEFAULT_OWNER_APP,
    projectSchema: DEFAULT_SCHEMA,
    policyPaths: [],
    json: false,
    verbose: false,
    approved: false,
    explain: false,
    strict: false,
    resolveCommit: false,
    resolveRollback: false,
    rollbackAll: false,
    force: false,
    backupAcknowledged: false,
    databaseUrlFromFlag: false,
    explicitConfig: {},
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
      // A following flag is a forgotten value, not the value. Consuming it silently
      // is worse than failing: `new add_users --dir --json` would write the migration
      // to ./--json and still exit 0. The inline form passes a literal dash-leading
      // value when one is genuinely meant.
      if (next.startsWith("--")) {
        throw new CliError(
          `flag --${key} needs a value, but the next argument is the flag ${next}; ` +
            `write --${key}=${next} to pass it as a literal value`,
        );
      }
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
        args.explicitConfig.dir = args.dir;
        break;
      case "database-url":
        args.databaseUrl = takeVal();
        args.databaseUrlFromFlag = true;
        args.explicitConfig.databaseUrl = args.databaseUrl;
        break;
      case "journal":
        args.journalPath = takeVal();
        break;
      case "tls-ca":
        args.tlsCaPath = takeVal();
        break;
      case "host-allowlist":
        args.hostAllowlist = takeVal();
        break;
      case "query-timeout":
        args.queryTimeoutMs = takeVal();
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
        args.explicitConfig.registryPath = args.registryPath;
        break;
      case "policy":
        args.explicitConfig.policyPaths ??= [];
        args.policyPaths.push(takeVal());
        (args.explicitConfig.policyPaths as string[]).push(args.policyPaths.at(-1)!);
        break;
      case "owner-app":
        args.ownerApp = takeVal();
        args.explicitConfig.ownerApp = args.ownerApp;
        break;
      case "schema":
        args.projectSchema = takeVal();
        args.explicitConfig.projectSchema = args.projectSchema;
        break;
      case "config":
        args.configPath = takeVal();
        break;
      case "env":
        args.environment = takeVal();
        break;
      case "json":
        rejectInlineVal();
        args.json = true;
        break;
      case "verbose":
        rejectInlineVal();
        args.verbose = true;
        break;
      case "approve":
        rejectInlineVal();
        args.approved = true;
        break;
      case "explain":
        rejectInlineVal();
        args.explain = true;
        break;
      case "strict":
        rejectInlineVal();
        args.strict = true;
        break;
      case "commit":
        rejectInlineVal();
        args.resolveCommit = true;
        break;
      case "rollback":
        rejectInlineVal();
        args.resolveRollback = true;
        break;
      case "to":
        args.rollbackTo = takeVal();
        break;
      case "steps":
        args.rollbackSteps = takeVal();
        break;
      case "all":
        rejectInlineVal();
        args.rollbackAll = true;
        break;
      case "force":
        rejectInlineVal();
        args.force = true;
        break;
      case "backup-acknowledged":
        rejectInlineVal();
        args.backupAcknowledged = true;
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
    args.command !== "resolve" &&
    args.positional !== undefined
  ) {
    throw new CliError(
      `command ${JSON.stringify(args.command)} does not accept positional arguments; use --dir`,
    );
  }
  if (args.dialect !== undefined && args.command !== "lint") {
    throw new CliError("flag --dialect is only valid with the lint command");
  }
  if ((args.resolveCommit || args.resolveRollback) && args.command !== "resolve") {
    throw new CliError("flags --commit and --rollback are only valid with resolve");
  }
  if (args.explain && args.command !== "lint") {
    throw new CliError("flag --explain is only valid with lint");
  }
  if (args.strict && args.command !== "status") {
    throw new CliError("flag --strict is only valid with status");
  }
  // `--json` selects a machine-readable reply, and three commands have none:
  // `new` writes a file, `apply` and `resolve` report progress as prose. Silently
  // accepting it there is worse than it sounds - the command succeeds, stdout
  // carries the human summary, and the pipeline that piped it into a parser gets
  // a syntax error naming the summary rather than the flag. Refused here for the
  // same reason every flag above is: the CLI already tells an operator which
  // commands a flag belongs to, and this was the one that did not.
  if (
    args.json &&
    args.command !== "lint" &&
    args.command !== "plan" &&
    args.command !== "status" &&
    args.command !== "rollback" &&
    args.command !== "history" &&
    args.command !== "version"
  ) {
    throw new CliError(
      "flag --json is only valid with lint, plan, status, rollback, history, or version",
    );
  }
  // `--approve` authorises destructive work, and only `apply`, `rollback` and
  // `resolve` consume it. `new`, `lint`, `plan`, `status` and `history` took it
  // and did nothing, which is the worst member of this class to leave silent:
  // `zero-migrate plan --approve` reads like pre-approving the plan it prints, and
  // approves nothing. Refused for the same reason as `--json` above.
  if (
    args.approved &&
    args.command !== "apply" &&
    args.command !== "rollback" &&
    args.command !== "resolve"
  ) {
    throw new CliError("flag --approve is only valid with apply, rollback, or resolve");
  }
  if (
    args.registryPath !== undefined &&
    args.command !== "lint" &&
    args.command !== "plan" &&
    args.command !== "apply" &&
    args.command !== "status" &&
    args.command !== "rollback" &&
    args.command !== "resolve"
  ) {
    throw new CliError(
      "flag --registry is only valid with lint, plan, apply, status, rollback, or resolve",
    );
  }
  if (
    args.policyPaths.length > 0 &&
    args.command !== "lint" &&
    args.command !== "plan" &&
    args.command !== "apply" &&
    args.command !== "status" &&
    args.command !== "history" &&
    args.command !== "rollback" &&
    args.command !== "resolve"
  ) {
    throw new CliError(
      "flag --policy is only valid with lint, plan, apply, status, history, rollback, or resolve",
    );
  }
  if (
    args.journalPath !== undefined &&
    args.command !== "apply" &&
    args.command !== "rollback"
  ) {
    throw new CliError("flag --journal is only valid with apply or rollback");
  }
  if (
    (args.rollbackTo !== undefined ||
      args.rollbackSteps !== undefined ||
      args.rollbackAll ||
      args.force ||
      args.backupAcknowledged) &&
    args.command !== "rollback"
  ) {
    throw new CliError(
      "flags --to, --steps, --all, --force, and --backup-acknowledged are only valid with rollback",
    );
  }
  return args;
}

/** A CLI-level failure with a clean message (mapped to a non-zero exit, no stack). */
class CliError extends Error {}

/** Apply config/environment/default precedence after raw flags have been parsed. */
function resolveParsedArgs(args: Args): Args {
  const resolved = resolveCliConfig({
    explicit: args.explicitConfig,
    configPath: args.configPath,
    environment: args.environment,
    defaults: {
      dir: DEFAULT_DIR,
      ownerApp: DEFAULT_OWNER_APP,
      projectSchema: DEFAULT_SCHEMA,
      policyPaths: [],
    },
  });
  // Checked HERE, after precedence has collapsed the three sources into one, so
  // the flag, `ZERO_MIGRATE_SCHEMA`, and a config `schema` field are all covered by
  // one rule. An empty value does not fall back to `DEFAULT_SCHEMA`: it overrides
  // it, which is what makes an unset CI variable (`--schema "$DEPLOY_SCHEMA"`)
  // dangerous rather than merely wrong.
  //
  // Without this, nothing downstream agreed on what an empty schema meant. SQLite
  // applied it cleanly because schema is inert there; PostgreSQL bootstrapped a
  // journal schema literally named `_migrations` and only then failed on a
  // zero-length delimited identifier; MySQL got `Incorrect database name ''` from
  // the server. Every other required setting -- the URL, `--registry`, `--policy`,
  // `--journal` -- already refuses a zero-length value by name.
  if (resolved.projectSchema.length === 0) {
    throw new CliError(
      "project schema must be non-empty (set --schema, ZERO_MIGRATE_SCHEMA, or the config `schema` field)",
    );
  }
  args.databaseUrl = resolved.databaseUrl;
  args.dir = resolved.dir;
  args.ownerApp = resolved.ownerApp;
  args.projectSchema = resolved.projectSchema;
  args.registryPath = resolved.registryPath;
  args.policyPaths = resolved.policyPaths;
  args.security = resolveNetworkSecurity(args, process.env);
  for (const warning of resolved.warnings) {
    process.stderr.write(`WARNING: ${warning}\n`);
  }
  return args;
}

/** Detect credentials embedded in the authority of an explicit network URL. */
export function hasInlinePassword(databaseUrl: string): boolean {
  const match = /^[a-z][a-z0-9+.-]*:\/\/([^/?#]*)/i.exec(databaseUrl.trim());
  if (match === null) return false;
  const at = match[1].lastIndexOf("@");
  return at > 0 && match[1].slice(0, at).includes(":");
}

function redactUrlTokens(message: string): string {
  return message.replace(
    /\b[a-z][a-z0-9+.-]*:\/\/[^\s"'()<>{}\[\]]+/gi,
    "<redacted database URL>",
  );
}

/** Keep connection failures useful without ever echoing the configured URL or
 * its credentials. */
function safeErrorMessage(error: unknown, databaseUrl: string | undefined): string {
  let message = (error as Error).message ?? String(error);
  if (databaseUrl === undefined || databaseUrl.length === 0) {
    return redactUrlTokens(message);
  }
  message = message.split(databaseUrl).join("<redacted database URL>");
  const authority = /^[a-z][a-z0-9+.-]*:\/\/([^/?#]*)/i.exec(databaseUrl.trim())?.[1];
  if (authority !== undefined) {
    const at = authority.lastIndexOf("@");
    if (at > 0) {
      const credentials = authority.slice(0, at);
      message = message.split(credentials).join("<redacted credentials>");
      const colon = credentials.indexOf(":");
      const username = colon === -1 ? credentials : credentials.slice(0, colon);
      const password = colon === -1 ? "" : credentials.slice(colon + 1);
      // Replaced only where the surrounding text marks it as a CREDENTIAL, not
      // everywhere the word appears. A plain substring replace rewrote the engine's
      // own diagnostics whenever the username collided with a word in them, and
      // `postgres` -- the default superuser -- collides with the dialect name the
      // engine reports, turning `dialect=postgres` into `dialect=<redacted user>`.
      // That is the one token in a three-dialect error that says which target
      // refused, so the conservative choice was destroying the message it protected.
      //
      // The whole URL, the `user:pass` pair and the password are each still redacted
      // in full above and below; what changes here is only the BARE word.
      if (username.length >= 3) {
        const escaped = username.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        // `user@host` and `user:password` — the URL authority forms.
        message = message.replace(new RegExp(`${escaped}(?=[@:])`, "g"), "<redacted user>");
        // How a server names the user back, in any quoting it chooses:
        //
        //   PostgreSQL  password authentication failed for user "name"
        //   MySQL       Access denied for user 'name'@'host'
        //   libpq       user=name
        //
        // All three are credential-shaped, and none is `user@` or `user:`. The
        // quote character is captured and back-referenced so the pair stays
        // symmetric and an unquoted keyword form still matches with it empty.
        //
        // Enumerating only the double-quoted form here is precisely how the first
        // attempt at this fix regressed: it stopped redacting the username in
        // PostgreSQL's auth error, and the password suite could not notice because
        // no password appears in that message.
        message = message.replace(
          new RegExp(`\\b(user|role)(\\s*=\\s*|\\s+)(['"\`]?)${escaped}\\3`, "gi"),
          "$1$2$3<redacted user>$3",
        );
      }
      if (password.length > 0) message = message.split(password).join("<redacted password>");
    }
  }
  return redactUrlTokens(message);
}

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

/** Read the ordered operator-controlled policy documents required by
 * database-backed commands. The bytes and occurrence order pass through
 * unchanged for Rust to parse and compose. */
async function loadPolicyFiles(paths: readonly string[]): Promise<string[]> {
  if (paths.length === 0) {
    throw new CliError(
      "missing policy (pass --policy <file> with an injecting or explicit no-inject policy)",
    );
  }

  const charterLayers: string[] = [];
  for (const path of paths) {
    if (path.length === 0) {
      throw new CliError("flag --policy needs a non-empty file path");
    }
    try {
      const source = await readFile(path, "utf8");
      if (source.length === 0) {
        throw new CliError(`policy file ${path} is empty`);
      }
      charterLayers.push(source);
    } catch (error) {
      if (error instanceof CliError) throw error;
      throw new CliError(`read policy file ${path}: ${(error as Error).message}`);
    }
  }
  return charterLayers;
}

/** Lint remains usable without a config file; in that case preview rendering is
 * shaped by an explicit in-memory no-inject charter. */
async function loadLintPolicyFiles(paths: readonly string[]): Promise<string[]> {
  return paths.length === 0 ? [NO_INJECT_POLICY] : await loadPolicyFiles(paths);
}

/** Derive the separate SQLite migration-journal filename next to the app DB. */
function sqliteJournalPath(appPath: string): string {
  const extension = extname(appPath);
  if (extension.length === 0) return `${appPath}.migrations`;
  return `${appPath.slice(0, -extension.length)}.migrations${extension}`;
}

function nonEmptyEnv(value: string | undefined): string | undefined {
  return value === undefined || value.trim().length === 0 ? undefined : value;
}

/** Build the host-enforced transport controls from flags (highest precedence),
 *  then env. `--tls-ca` / `ZERO_MIGRATE_TLS_CA` is a FILE PATH whose PEM contents
 *  are pinned (not the PEM itself). Returns `undefined` when no control is set, so
 *  the network driver default is unchanged. These apply only to `postgres`/`mysql`
 *  drivers (the host owns the socket); SQLite runs in-process. */
export function resolveNetworkSecurity(
  args: Pick<Args, "tlsCaPath" | "hostAllowlist" | "queryTimeoutMs">,
  processEnv: NodeJS.ProcessEnv,
): NetworkSecurityOptions | undefined {
  const caPath = args.tlsCaPath ?? nonEmptyEnv(processEnv.ZERO_MIGRATE_TLS_CA);
  const allowlistRaw = args.hostAllowlist ?? nonEmptyEnv(processEnv.ZERO_MIGRATE_HOST_ALLOWLIST);
  const timeoutRaw = args.queryTimeoutMs ?? nonEmptyEnv(processEnv.ZERO_MIGRATE_QUERY_TIMEOUT_MS);

  const security: NetworkSecurityOptions = {};
  if (caPath !== undefined) {
    try {
      security.tlsCa = readFileSync(caPath, "utf8");
    } catch (e) {
      throw new CliError(
        `--tls-ca: cannot read CA bundle ${JSON.stringify(caPath)}: ${(e as Error).message}`,
      );
    }
  }
  if (allowlistRaw !== undefined) {
    const hosts = allowlistRaw
      .split(",")
      .map((h) => h.trim())
      .filter((h) => h.length > 0);
    if (hosts.length > 0) security.hostAllowlist = hosts;
  }
  if (timeoutRaw !== undefined) {
    const ms = Number(timeoutRaw);
    if (!Number.isInteger(ms) || ms <= 0) {
      throw new CliError(
        `--query-timeout must be a positive integer (ms); got ${JSON.stringify(timeoutRaw)}`,
      );
    }
    security.queryTimeoutMs = ms;
  }
  return Object.keys(security).length > 0 ? security : undefined;
}

/** Select the supported Node driver from a database URL scheme. */
export function driverFor(
  databaseUrl: string,
  journalOverride?: string,
  security?: NetworkSecurityOptions,
): DriverConfig {
  const trimmed = databaseUrl.trimStart();
  const lower = trimmed.toLowerCase();
  const hasUriScheme = /^[a-z][a-z0-9+.-]*:/i.test(trimmed);
  const isWindowsDrivePath = /^[a-z]:[\\/]/i.test(trimmed);
  if (lower.startsWith("postgres://") || lower.startsWith("postgresql://")) {
    if (journalOverride !== undefined) {
      throw new CliError("flag --journal is only valid for a SQLite database URL");
    }
    return { kind: "postgres", url: databaseUrl, security };
  }
  if (lower.startsWith("mysql://")) {
    if (journalOverride !== undefined) {
      throw new CliError("flag --journal is only valid for a SQLite database URL");
    }
    return { kind: "mysql", url: databaseUrl, security };
  }
  const isBareSqlitePath =
    (!hasUriScheme || isWindowsDrivePath) &&
    (lower.endsWith(".sqlite") || lower.endsWith(".db"));
  if (lower.startsWith("sqlite:") || isBareSqlitePath) {
    const appPath = lower.startsWith("sqlite:")
      ? trimmed.replace(/^sqlite:(?:\/\/)?/i, "")
      : trimmed;
    if (appPath.length === 0) {
      throw new CliError("SQLite database URL needs an application database path");
    }
    if (journalOverride !== undefined && journalOverride.length === 0) {
      throw new CliError("flag --journal needs a non-empty file path");
    }
    return {
      kind: "sqlite",
      appPath,
      journalPath: journalOverride ?? sqliteJournalPath(appPath),
    };
  }
  if (journalOverride !== undefined) {
    throw new CliError("flag --journal is only valid for a SQLite database URL");
  }
  throw new CliError(
    "could not infer a driver from the database URL (expected a postgres:// or mysql:// scheme, or sqlite:<path>)",
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

/** Import the complete ordered migration set without executing any authoring phase. */
async function importMigrations(files: readonly MigrationFile[]): Promise<LoadedMigration[]> {
  const loaded: LoadedMigration[] = [];
  for (const file of files) {
    loaded.push({ file, migration: await importMigration(file.path) });
  }
  return loaded;
}

/** Record every trusted migration exactly once and keep file/module/envelope aligned. */
function authorMigrations(
  migrations: readonly LoadedMigration[],
): Array<LoadedMigration & { envelope: IrEnvelope }> {
  const irVersion = currentIrVersion();
  return migrations.map(({ file, migration }) => ({
    file,
    migration,
    envelope: buildEnvelope(migration, { irVersion, nameFallback: file.label }),
  }));
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
  schema() {
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

/** One dialect-specific offline verification result. */
interface LintDialectResult {
  dialect: Dialect;
  ok: boolean;
  irVersion?: number;
  opCount?: number;
  error?: string;
  sql?: string;
}

/** One aggregate `lint` verdict line. */
interface LintLine {
  label: string;
  ok: boolean;
  opCount: number;
  dialects: LintDialectResult[];
}

/** `lint` verifies each authored envelope for all requested dialects and asks
 * the policy-aware preview renderer to lower the same bytes. Rendering is
 * isolated per envelope so a lowering failure remains a migration verdict. */
async function runLint(args: Args): Promise<number> {
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const registry = await loadRegistry(args.registryPath);
  const charterLayers = await loadLintPolicyFiles(args.policyPaths);
  const migrations = await importMigrations(files);
  assertUniqueMigrationNames(migrations);
  const authored = authorMigrations(migrations);
  const envelopeJson = authored.map(({ envelope }) => JSON.stringify(envelope));
  const selectedDialects: readonly Dialect[] = args.dialect
    ? [args.dialect]
    : ALL_DIALECTS;
  const addon = loadAddon();

  const reports: LintLine[] = authored.map(({ file, envelope }, index) => {
    const dialects = selectedDialects.map((dialect): LintDialectResult => {
      const verdict = addon.loadVerify(
        envelopeJson[index],
        args.ownerApp,
        dialect,
        registry,
        args.projectSchema,
      );
      let renderedSql: string | undefined;
      let previewError: string | undefined;
      try {
        // The whole prefix, not this file alone. A migration's ops are only
        // checkable against the schema the earlier migrations leave behind: lint
        // reported ok on a backfill whose cursor column a `createTable` two files
        // earlier declares with a type apply refuses, because that column was
        // never in view (F653). The preview folds the prefix and renders the last
        // envelope against it, so lint reaches the planner's own verdict.
        const rendered = previewSql({
          envelopes: envelopeJson.slice(0, index + 1),
          dialect,
          defaultSchema: args.projectSchema,
          ownerApp: args.ownerApp,
          charterLayers,
        });
        renderedSql = rendered[rendered.length - 1];
      } catch (error) {
        previewError = safeErrorMessage(error, undefined);
      }
      return {
        dialect,
        ok: verdict.ok && previewError === undefined,
        irVersion: verdict.irVersion,
        opCount: verdict.opCount,
        error: verdict.error ?? previewError,
        ...(args.explain && renderedSql !== undefined ? { sql: renderedSql } : {}),
      };
    });
    return {
      label: envelope.name || file.label,
      ok: dialects.every((result) => result.ok),
      opCount: envelope.ops.length,
      dialects,
    };
  });

  // Same reason as plan: an advisory that only exists in the human rendering is
  // invisible to the gate that reads this output.
  const advisories = args.explain
    ? addon.advisoriesFor({
        envelopes: envelopeJson,
        dialect: selectedDialects[0],
        defaultSchema: args.projectSchema,
        ownerApp: args.ownerApp,
        charterLayers,
      })
    : [];

  if (args.json) {
    process.stdout.write(
      JSON.stringify(args.explain ? { reports, advisories } : reports, null, 2) + "\n",
    );
  } else {
    for (const report of reports) {
      process.stdout.write(
        `lint ${report.label}: ${report.ok ? "ok" : "fail"} (${report.opCount} ops)\n`,
      );
      for (const result of report.dialects) {
        if (!result.ok) {
          process.stdout.write(`  ${result.dialect}: ${result.error ?? "verification failed"}\n`);
        }
        if (args.explain) process.stdout.write(`${result.sql ?? ""}\n`);
      }
    }
    // Under --explain the operator is asking what this deploy will actually do,
    // which is exactly when a table-wide lock is worth knowing about.
    if (args.explain) writeAdvisories(advisories);
  }
  return reports.every((report) => report.ok) ? 0 : 1;
}

export interface PendingMigrationPreview {
  version: string;
  name: string;
  envelope: IrEnvelope;
}

/** Correlate top-level pending logical plan IDs back to authored envelopes in
 * source order. StatusIr always returns plan detail; fail closed if it does not. */
export function pendingMigrationsForPlan(
  reply: StatusReply,
  envelopes: readonly IrEnvelope[],
): PendingMigrationPreview[] {
  const pending = new Set(reply.pending);
  const plans = reply.plans ?? [];
  const byName = new Map<string, (typeof plans)[number]>();
  for (const plan of plans) {
    if (byName.has(plan.name)) {
      throw new CliError(`status returned ambiguous migration name ${JSON.stringify(plan.name)}`);
    }
    byName.set(plan.name, plan);
  }
  const result: PendingMigrationPreview[] = [];
  for (const envelope of envelopes) {
    const plan = byName.get(envelope.name);
    if (plan !== undefined && pending.has(plan.version)) {
      result.push({ version: plan.version, name: plan.name, envelope });
    }
  }
  const correlated = new Set(result.map(({ version }) => version));
  const missing = reply.pending.filter((version) => !correlated.has(version));
  if (missing.length > 0) {
    throw new CliError(
      `status returned pending migration(s) that cannot be matched to source: ${missing.join(", ")}`,
    );
  }
  return result;
}

/** `plan` connects only for status reconciliation, then renders pending envelope
 * SQL offline. It never invokes apply or resolution. */
async function runLivePlan(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError("missing database URL (pass --database-url or set DATABASE_URL)");
  }
  const driver = driverFor(args.databaseUrl, undefined, args.security);
  const charterLayers = await loadPolicyFiles(args.policyPaths);
  const registry = await loadRegistry(args.registryPath);
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const migrations = await importMigrations(files);
  assertUniqueMigrationNames(migrations);
  const authored = authorMigrations(migrations);
  const envelopes = authored.map(({ envelope }) => envelope);
  const reply = await statusEnvelopes({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    driver,
    registry,
    policy: charterLayers,
    envelopes,
    readOnly: true,
  });
  // The pending set is what `plan` renders, and a busy reply has none because it
  // read none. Rendering the empty set would print "would apply 0 migrations" for
  // a database nobody looked at.
  if (reply.busy) {
    process.stderr.write(formatStatusBusy(reply, "plan"));
    return 0;
  }
  const pending = pendingMigrationsForPlan(reply, envelopes);
  const rendered = previewSql({
    envelopes: pending.map(({ envelope }) => JSON.stringify(envelope)),
    dialect: driver.kind,
    defaultSchema: args.projectSchema,
    ownerApp: args.ownerApp,
    charterLayers,
  });

  // Computed ONCE, for both output shapes. Surfacing advisories only in the
  // human branch would leave the machine consumer - CI, which is precisely where
  // a table-wide lock warning has to land - reading a payload that looks clean.
  const advisories = loadAddon().advisoriesFor({
    envelopes: pending.map(({ envelope }) => JSON.stringify(envelope)),
    dialect: driver.kind,
    defaultSchema: args.projectSchema,
    ownerApp: args.ownerApp,
    charterLayers,
  });

  if (args.json) {
    process.stdout.write(
      JSON.stringify(
        {
          count: pending.length,
          pending: pending.map(({ version, name }, index) => ({
            version,
            name,
            sql: rendered[index],
          })),
          advisories,
        },
        null,
        2,
      ) + "\n",
    );
  } else {
    const noun = pending.length === 1 ? "migration" : "migrations";
    process.stdout.write(`would apply ${pending.length} ${noun}\n`);
    for (const sql of rendered) process.stdout.write(`${sql}\n`);
    writeAdvisories(advisories);
  }
  return 0;
}

/** Print the analyzer's operational advisories for an about-to-run plan.
 *
 * F650. The engine computed these all along -- an ACCESS EXCLUSIVE warning for
 * `ALTER TABLE ... ADD CONSTRAINT ... UNIQUE` among them -- and no verb ever
 * read one, so an operator adding a unique column to a populated table took a
 * table-wide lock with nothing telling them it was coming.
 *
 * The STATEMENT is printed with the message because the analyzer names the
 * constraint and not the table, and "which table locks" is the question being
 * answered. These never gate: the exit code is untouched, or an advisory would
 * be a denial wearing a softer word. */
function writeAdvisories(advisories: readonly AdvisoryDto[]): void {
  for (const advisory of advisories) {
    // Statement and message on ONE line. The analyzer names the CONSTRAINT
    // ("lk_t_e_key") and the statement names the TABLE, and an operator scanning
    // for which table is about to lock needs both in the same place -- splitting
    // them across lines is how a warning becomes something you grep past.
    const statement = advisory.statement.replace(/\s+/g, " ").trim();
    process.stdout.write(
      `advisory [${advisory.severity}] ${advisory.rule}: ${statement} -- ${advisory.message}\n` +
        (advisory.suggestion ? `  suggestion: ${advisory.suggestion}\n` : ""),
    );
  }
}

/** `apply [dir]` — apply every migration over the `--database-url` driver in order. */
async function runApply(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError(
      "missing database URL (pass --database-url or set DATABASE_URL)",
    );
  }
  const driver = driverFor(args.databaseUrl, args.journalPath, args.security);
  const charterLayers = await loadPolicyFiles(args.policyPaths);
  const registry = await loadRegistry(args.registryPath);
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const migrations = await importMigrations(files);
  assertUniqueMigrationNames(migrations);
  for (const [index, { file, migration }] of migrations.entries()) {
    const prior = migrations.slice(0, index);
    const outcome = await apply({
      migration,
      priorMigrations: prior.map((entry) => entry.migration),
      priorNameFallbacks: prior.map((entry) => entry.file.label),
      ownerApp: args.ownerApp,
      projectSchema: args.projectSchema,
      driver,
      registry,
      policy: charterLayers,
      nameFallback: file.label,
      approved: args.approved,
    });
    process.stdout.write(`apply ${file.label}: ${JSON.stringify(outcome)}\n`);
  }
  return 0;
}

/**
 * Turn the three target flags into the one target the addon takes.
 *
 * Exactly one is required. Zero is refused because there is no safe default for
 * how much of a schema to tear down, and more than one because the operator meant
 * one of them and picking for them would unwind the wrong amount. Kept pure so
 * both rules stay host-testable without a database.
 */
export function rollbackTargetFromArgs(args: {
  rollbackTo?: string;
  rollbackSteps?: string;
  rollbackAll: boolean;
}): RollbackTargetDto {
  const chosen = [
    args.rollbackTo !== undefined ? "--to" : undefined,
    args.rollbackSteps !== undefined ? "--steps" : undefined,
    args.rollbackAll ? "--all" : undefined,
  ].filter((flag): flag is string => flag !== undefined);
  if (chosen.length === 0) {
    throw new CliError(
      "rollback needs a target: --to <version>, --steps <n>, or --all",
    );
  }
  if (chosen.length > 1) {
    throw new CliError(
      `rollback takes one target, but ${chosen.join(" and ")} were both given`,
    );
  }
  if (args.rollbackTo !== undefined) {
    return { kind: "toVersion", version: args.rollbackTo };
  }
  if (args.rollbackSteps !== undefined) {
    // `Number("")` is 0 and `Number(" 2 ")` is 2, so the digits are checked before
    // the conversion: `--steps=` must not read as a silent no-op unwind.
    const steps = /^\d+$/.test(args.rollbackSteps) ? Number(args.rollbackSteps) : NaN;
    if (!Number.isInteger(steps)) {
      throw new CliError(
        `flag --steps needs a non-negative whole number; got ${JSON.stringify(args.rollbackSteps)}`,
      );
    }
    return { kind: "steps", steps };
  }
  return { kind: "all" };
}

/** Unwind applied migrations, newest first, down to the requested target. */
async function runRollback(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError(
      "missing database URL (pass --database-url or set DATABASE_URL)",
    );
  }
  const target = rollbackTargetFromArgs(args);
  if (!args.approved) {
    throw new CliError(
      "rollback runs the reverse SQL of applied migrations, so it needs --approve",
    );
  }
  const driver = driverFor(args.databaseUrl, args.journalPath, args.security);
  const charterLayers = await loadPolicyFiles(args.policyPaths);
  const registry = await loadRegistry(args.registryPath);
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const loaded = await importMigrations(files);
  assertUniqueMigrationNames(loaded);
  // The WHOLE authored set goes over, not a prefix: the addon reconstructs each
  // `down` from its envelope, and a migration left out has no reverse SQL, which
  // the engine reports as a refusal rather than a skip.
  const outcome = await rollback({
    migrations: loaded.map((entry) => entry.migration),
    nameFallbacks: loaded.map((entry) => entry.file.label),
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    driver,
    registry,
    policy: charterLayers,
    target,
    approved: args.approved,
    force: args.force,
    backupAcknowledged: args.backupAcknowledged,
  });
  process.stdout.write(
    args.json
      ? `${JSON.stringify(outcome, null, 2)}\n`
      : `${formatRollbackHuman(outcome)}`,
  );
  return 0;
}

/** The operator lines for a completed rollback. */
export function formatRollbackHuman(outcome: RollbackOutcome): string {
  const lines = [`rollback: ${outcome.rolledBack.length} rolled back`];
  // `advisories` arrives from a newer addon than some callers construct. An
  // outcome without the field is a rollback that raised none, not a crash in the
  // formatter that reports the rollback.
  for (const advisory of outcome.advisories ?? []) lines.push(`  advisory: ${advisory}`);
  for (const version of outcome.rolledBack) lines.push(`  reversed ${version}`);
  for (const version of outcome.skippedIrreversible) {
    lines.push(`  skipped ${version} (declares no down; crossed under --force)`);
  }
  if (outcome.rolledBack.length === 0 && outcome.skippedIrreversible.length === 0) {
    lines.push("  nothing was applied in the requested range");
  }
  return `${lines.join("\n")}\n`;
}

/** How much of a lock holder's current statement the operator line shows. */
const HOLDER_QUERY_LIMIT = 200;

/**
 * The operator line for a status or plan that found the project lock held.
 *
 * Names the holding session so the reader knows who to go ask, and says plainly
 * that nothing was read -- a caller must not mistake the empty reply for a clean
 * database. It reports no duration: `pg_locks` records no acquisition time, so
 * every timestamp available would age the holder's session or statement rather
 * than the lock.
 *
 * Kept pure and separate from the exit-code rule so both stay host-testable.
 */
export function formatStatusBusy(reply: StatusReply, verb: string): string {
  const lines = [
    `zero-migrate: another deploy holds the project lock; ${verb} read nothing and did not wait for it`,
  ];
  for (const holder of reply.lockHolders) {
    const attributes = [holder.applicationName, holder.state]
      .filter((value) => value !== undefined && value !== null && value !== "")
      .join(", ");
    const suffix = attributes === "" ? "" : ` (${attributes})`;
    const query = holder.query ?? "";
    const shown =
      query.length > HOLDER_QUERY_LIMIT
        ? `${query.slice(0, HOLDER_QUERY_LIMIT)}...`
        : query;
    lines.push(
      `zero-migrate:   held by pid ${holder.pid}${suffix}` +
        (shown === "" ? "" : `: ${redactUrlTokens(shown)}`),
    );
  }
  if (reply.lockHolders.length === 0) {
    lines.push(
      "zero-migrate:   the holding session could not be identified; it may have finished",
    );
  }
  return `${lines.join("\n")}\n`;
}

/** True when strict status should fail. */
export function statusIsDirty(reply: StatusReply): boolean {
  return (
    reply.pending.length > 0 ||
    reply.pendingContracts.length > 0 ||
    reply.blocked.length > 0 ||
    reply.unexpectedJournal.length > 0 ||
    (reply.plans ?? []).some(
      (plan) =>
        plan.state === "drifted" || plan.steps.some((step) => step.state === "drifted"),
    )
  );
}

/**
 * Status exit code policy, kept pure so CI semantics are host-testable.
 *
 * Callers must branch on `reply.busy` BEFORE reaching here: a busy reply carries
 * no reconciled state, so asking it whether the migration set is dirty is asking
 * a question it did not answer.
 */
export function statusExitCode(reply: StatusReply, strict: boolean): number {
  return strict && statusIsDirty(reply) ? 1 : 0;
}

/** Preserve the addon's structured reply exactly for `status --json`. */
export function formatStatusJson(reply: StatusReply): string {
  return `${JSON.stringify(reply, null, 2)}\n`;
}

/** Human status with stable count, drift, and checksum-mismatch lines.
 *
 * A busy reply renders as the contention notice rather than as counts: every
 * count in it is zero because nothing was read, and printing
 * "0 applied, 0 pending" for a database nobody looked at would be a lie. */
export function formatStatusHuman(reply: StatusReply): string {
  if (reply.busy) return formatStatusBusy(reply, "status");
  const lines = [
    `status: ${reply.applied.length} applied, ${reply.pending.length} pending`,
  ];
  for (const entry of reply.unexpectedJournal) {
    lines.push(`drift: unexpected journal entry ${entry.version} (${entry.state})`);
  }
  for (const plan of reply.plans ?? []) {
    const driftedSteps = plan.steps.filter((step) => step.state === "drifted");
    if (driftedSteps.length === 0 && plan.state === "drifted") {
      lines.push(`checksum mismatch: ${plan.name} (${plan.version})`);
    }
    for (const step of driftedSteps) {
      lines.push(
        `checksum mismatch: ${plan.name}, step ${step.name} (${step.version})`,
      );
    }
  }
  for (const contract of reply.pendingContracts) {
    lines.push(
      `pending online rename: ${contract.table} (${contract.pendingVersion})${contract.orphaned ? " [orphaned]" : ""}`,
    );
  }
  for (const blocked of reply.blocked) {
    lines.push(
      `blocked: ${blocked.blocked} waits for ${blocked.dependency} (${blocked.pendingVersion})`,
    );
  }
  return `${lines.join("\n")}\n`;
}

/** `status [dir]` reconciles the authored set against the live journal. */
async function runStatus(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError(
      "missing database URL (pass --database-url or set DATABASE_URL)",
    );
  }
  const driver = driverFor(args.databaseUrl, undefined, args.security);
  const charterLayers = await loadPolicyFiles(args.policyPaths);
  const registry = await loadRegistry(args.registryPath);
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const migrations = await importMigrations(files);
  assertUniqueMigrationNames(migrations);
  const envelopes = authorMigrations(migrations).map(({ envelope }) => envelope);
  const reply = await statusEnvelopes({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    driver,
    registry,
    policy: charterLayers,
    envelopes,
    readOnly: false,
  });
  // Contention is not a dirty migration set. A strict gate that exited 1 here
  // would fail every pipeline that happens to overlap a deploy, so the verdict is
  // "no answer", not "bad answer" -- and the machine-readable `busy` flag in the
  // JSON reply is how a CI that WANTS to fail on contention opts in.
  if (reply.busy) {
    process.stderr.write(formatStatusBusy(reply, "status"));
    if (args.json) process.stdout.write(formatStatusJson(reply));
    return 0;
  }
  if (args.json) {
    process.stdout.write(formatStatusJson(reply));
  } else {
    process.stdout.write(formatStatusHuman(reply));
  }
  return statusExitCode(reply, args.strict);
}

/** Map a migration name to exactly one outstanding online-rename obligation. */
export function resolvePendingVersion(reply: StatusReply, migrationName: string): string {
  const plans = (reply.plans ?? []).filter((plan) => plan.name === migrationName);
  if (plans.length === 0) {
    throw new CliError(
      `unknown pending online-rename migration ${JSON.stringify(migrationName)}`,
    );
  }
  if (plans.length > 1) {
    throw new CliError(
      `ambiguous migration name ${JSON.stringify(migrationName)} in status reply`,
    );
  }
  if (!reply.pending.includes(plans[0].version)) {
    // "Not pending" is true but reads as "never ran", because everywhere else in
    // this CLI pending means not yet applied. The states that actually land here
    // are the opposite: an operator who resolved once and is retrying, or a
    // pipeline replaying a step. Naming the real state keeps a retry from looking
    // like a lost deploy.
    const name = JSON.stringify(migrationName);
    const version = plans[0].version;
    if (reply.aborted.includes(version)) {
      throw new CliError(
        `migration ${name} had its online rename rolled back already; there is nothing left to resolve`,
      );
    }
    if (reply.rolledBack.includes(version)) {
      throw new CliError(
        `migration ${name} has been rolled back; there is no outstanding online rename to resolve`,
      );
    }
    if (reply.applied.includes(version)) {
      // Covers both "already committed" and "never had a rename at all". The two
      // are indistinguishable from here -- a resolved contract leaves no trace in
      // the status reply -- so the wording is chosen to be true of both.
      throw new CliError(
        `migration ${name} is fully applied; there is no outstanding online rename to resolve`,
      );
    }
    throw new CliError(
      `migration ${name} is not pending (state: ${plans[0].state})`,
    );
  }
  const stepVersions = new Set(plans[0].steps.map((step) => step.version));
  const contracts = reply.pendingContracts.filter((contract) =>
    stepVersions.has(contract.pendingVersion),
  );
  if (contracts.length === 0) {
    throw new CliError(
      `migration ${JSON.stringify(migrationName)} has no pending online rename`,
    );
  }
  if (contracts.length > 1) {
    throw new CliError(
      `migration ${JSON.stringify(migrationName)} has multiple pending online renames; resolution is ambiguous`,
    );
  }
  return contracts[0].pendingVersion;
}

/** Complete or roll back one outstanding PostgreSQL online rename by migration name. */
async function runResolve(args: Args): Promise<number> {
  const migrationName = args.positional;
  if (!migrationName) {
    throw new CliError("`resolve` needs a migration name: zero-migrate resolve <migration>");
  }
  if (args.resolveCommit === args.resolveRollback) {
    throw new CliError("choose exactly one of --commit or --rollback");
  }
  if (!args.approved) {
    throw new CliError("resolve requires --approve after reviewing the column drop");
  }
  if (!args.databaseUrl) {
    throw new CliError("missing database URL (pass --database-url or set DATABASE_URL)");
  }
  const driver = driverFor(args.databaseUrl, undefined, args.security);
  if (driver.kind !== "postgres") {
    throw new CliError("resolve supports only PostgreSQL online renames");
  }
  const charterLayers = await loadPolicyFiles(args.policyPaths);
  const registry = await loadRegistry(args.registryPath);
  const files = await discover(args.dir);
  if (files.length === 0) throw new CliError(`no migrations found in ${args.dir}`);
  await ensureTsLoader(files);
  const migrations = await importMigrations(files);
  assertUniqueMigrationNames(migrations);
  const authored = authorMigrations(migrations);
  const localMatches = authored.filter(({ envelope }) => envelope.name === migrationName);
  if (localMatches.length === 0) {
    throw new CliError(`unknown migration ${JSON.stringify(migrationName)} in ${args.dir}`);
  }
  if (localMatches.length > 1) {
    throw new CliError(`ambiguous migration name ${JSON.stringify(migrationName)} in ${args.dir}`);
  }
  const reply = await statusEnvelopes({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    driver,
    registry,
    policy: charterLayers,
    envelopes: authored.map(({ envelope }) => envelope),
  });
  // `resolve` writes, so it cannot proceed on a reply that read nothing. Without
  // this the empty pending set would surface as "unknown migration", blaming the
  // operator's argument for a peer's deploy.
  if (reply.busy) {
    throw new CliError(
      formatStatusBusy(reply, "resolve").trim().replace(/^zero-migrate: /, ""),
    );
  }
  const pendingVersion = resolvePendingVersion(reply, migrationName);
  const outcome = await resolvePending({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    pendingVersion,
    action: args.resolveCommit ? "apply" : "abort",
    driver,
    approved: true,
    policy: charterLayers,
    appliedBy: "cli",
  });
  process.stdout.write(`resolve ${migrationName}: ${JSON.stringify(outcome)}\n`);
  return 0;
}

/** `history` prints the append-only migration audit trail over the
 *  `--database-url` driver. PostgreSQL only (the journal history verb is PG-backed). */
async function runHistory(args: Args): Promise<number> {
  if (!args.databaseUrl) {
    throw new CliError("missing database URL (pass --database-url or set DATABASE_URL)");
  }
  const driver = driverFor(args.databaseUrl, undefined, args.security);
  if (driver.kind !== "postgres") {
    throw new CliError("history supports only PostgreSQL");
  }
  const charterLayers = await loadPolicyFiles(args.policyPaths);
  const reply = await history({
    ownerApp: args.ownerApp,
    projectSchema: args.projectSchema,
    driver,
    policy: charterLayers,
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
  zero-migrate lint [--dir <dir>] [--dialect <name>] [--explain] [--registry <file>] [--policy <file> ...] [--json]
  zero-migrate plan [--dir <dir>] [--database-url <url>] [--policy <file> ...] [--registry <file>] [--json]
  zero-migrate apply [--dir <dir>] [--database-url <url>] [--policy <file> ...] [--journal <path>] [--registry <file>] [--approve]
  zero-migrate rollback (--to <version> | --steps <n> | --all) --approve [--dir <dir>] [--database-url <url>] [--policy <file> ...] [--journal <path>] [--registry <file>] [--force --backup-acknowledged] [--json]
  zero-migrate status [--dir <dir>] [--database-url <url>] [--policy <file> ...] [--registry <file>] [--strict] [--json]
  zero-migrate resolve <migration> (--commit | --rollback) --approve [--database-url <url>] [--policy <file> ...] [--registry <file>]
  zero-migrate history [--database-url <url>] [--policy <file> ...] [--json]
  zero-migrate --version

Flags:
  --dir <dir>           Migration directory (default ./migrations)
  --database-url <url>  postgres:// or mysql:// DSN, or sqlite:<path>
  --journal <path>      SQLite journal override (default: <app>.migrations.<ext>)
  --tls-ca <file>       Pin this PEM CA bundle for the postgres/mysql TLS connection
                        (env ZERO_MIGRATE_TLS_CA); verifies the server certificate
  --host-allowlist <csv> Reject connecting to any host not in this comma list
                        (env ZERO_MIGRATE_HOST_ALLOWLIST)
  --query-timeout <ms>  Per-verb query timeout for the network driver
                        (env ZERO_MIGRATE_QUERY_TIMEOUT_MS)
  --dialect <name>      lint only: postgres, mysql, or sqlite (default all three)
  --registry <file>     Trusted JSON map of table names to owner app IDs
  --policy <file>
                        Repeatable ordered TOML policy layer; first is the root/bound
                        Later layers may only narrow; only root may use mandatory injects
  --owner-app <app>     Deploying app id stamped as owner_app (default app_cli)
  --schema <schema>     Confined project schema (default public)
  --approve             Approve reviewed destructive changes and backfills
  --to <version>        rollback: unwind everything applied after this version
  --steps <n>           rollback: unwind the n most recently applied migrations
  --all                 rollback: unwind every applied migration
  --force               rollback: cross a migration with no down by skipping it
                        (needs --backup-acknowledged too)
  --backup-acknowledged rollback: state that a backup exists before forcing
  --commit              Resolve an online rename and keep the new column
  --rollback            Resolve an online rename and keep the old column
  --explain             lint: include rendered SQL for selected dialects
  --strict              status: fail if pending, drifted, or checksum-mismatched
  --json                Machine-readable output where supported
  --config <path>       Use this zero-migrate.toml instead of upward discovery
  --env <name>          Select [env.<name>] (default dev, or the sole block)
  --version             Print the zero-migrate version
  --verbose             version only: also report the engine build identity
  --help                This help

Configuration precedence is flag, ZERO_MIGRATE_* environment variable, selected
zero-migrate.toml environment, then default. DATABASE_URL remains the URL fallback.
Set ZERO_MIGRATE_LOG=1 for engine diagnostics on stderr (off by default; they never
touch the single JSON document --json writes to stdout).
Only lint accepts --dialect; live commands derive it from the URL. lint is offline.
plan, apply and rollback support PostgreSQL, MySQL 8, and SQLite; status supports
PostgreSQL and MySQL 8; history and resolve are PostgreSQL-only. rollback reverses
applied migrations from their authored down; there is no clean command.
`;

/** Entry point: parse, dispatch, map thrown `CliError` to a clean non-zero exit. */
export async function main(argv: string[]): Promise<number> {
  let args: Args;
  try {
    args = parseArgs(argv);
  } catch (e) {
    process.stderr.write(`zero-migrate: ${redactUrlTokens((e as Error).message)}\n`);
    return 1;
  }
  if (args.command === "version") {
    // Bare `version` stays one scalar and stays independent of the addon: it answers
    // even when the binary is missing or ZERO_MIGRATE_ADDON_PATH points at nothing,
    // and `$(zero-migrate version)` captures whatever is here in full.
    if (!args.verbose) {
      process.stdout.write(`${packageVersion()}\n`);
      return 0;
    }
    return versionVerbose(args.json);
  }
  if (args.command === "" || args.command === "help") {
    process.stdout.write(USAGE);
    return args.command === "" ? 1 : 0;
  }
  try {
    if (
      args.databaseUrlFromFlag &&
      args.databaseUrl !== undefined &&
      hasInlinePassword(args.databaseUrl)
    ) {
      process.stderr.write(
        "WARNING: --database-url contains an inline password; prefer DATABASE_URL or an env: reference in zero-migrate.toml.\n",
      );
    }
    args = resolveParsedArgs(args);
    switch (args.command) {
      case "new":
        return await runNew(args);
      case "lint":
        return await runLint(args);
      case "plan":
        return await runLivePlan(args);
      case "apply":
        return await runApply(args);
      case "rollback":
        return await runRollback(args);
      case "status":
        return await runStatus(args);
      case "history":
        return await runHistory(args);
      case "resolve":
        return await runResolve(args);
      default:
        process.stderr.write(
          `zero-migrate: unknown command ${JSON.stringify(redactUrlTokens(args.command))}\n`,
        );
        process.stdout.write(USAGE);
        return 1;
    }
  } catch (e) {
    process.stderr.write(`zero-migrate: ${safeErrorMessage(e, args.databaseUrl)}\n`);
    return 1;
  }
}
