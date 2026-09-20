/**
 * `zeroship.jsonc` - the creator project configuration, build side.
 *
 * SCOPE INVARIANT: this file is
 * read by the `zeroship` CLI and by the build toolchain. It is NEVER read by
 * the runtime and NEVER packed into a `.zship`. The packer walks `distDir` and
 * `zeroship.jsonc` lives one level above it, so the invariant holds by
 * construction. The package's archive tests assert that boundary against the
 * emitted archive bytes.
 *
 * THIS IS THE SIDE THAT HAS DEFAULTS. The plugin must work with
 * `zeroship()` and no file at all - that is what the scaffold ships - so every
 * schema `default` is applied here. The Rust CLI has none: a key it reads and
 * the file omits is an error naming the key. One default, one holder, no way
 * for the two readers to disagree.
 */

import { existsSync, lstatSync, readFileSync, realpathSync } from "node:fs";
import { basename, dirname, extname, isAbsolute, relative, resolve, sep } from "node:path";
import {
  getNodeValue,
  parseTree,
  printParseErrorCode,
  type Node,
  type ParseError,
} from "jsonc-parser";

import {
  APP_KNOWN_KEYS,
  APP_REQUIRED_KEYS,
  CLI_READ_FIELDS,
  CONFIG_ENV_VAR,
  CONFIG_FILENAME,
  DATABASE_KNOWN_KEYS,
  DATABASE_REQUIRED_KEYS,
  DEFAULTS,
  ENVIRONMENT_APP_KNOWN_KEYS,
  ENVIRONMENT_APP_REQUIRED_KEYS,
  ENVIRONMENT_DATABASE_KNOWN_KEYS,
  ENVIRONMENT_DATABASE_REQUIRED_KEYS,
  ENVIRONMENT_KNOWN_KEYS,
  ENVIRONMENT_REQUIRED_KEYS,
  FIELD_RULES,
  FORBIDDEN_KEY_NAMES,
  LABEL_RULES,
  ROOT_KNOWN_KEYS,
  SCHEMA_ID,
  ROOT_REQUIRED_KEYS,
  BUILD_KNOWN_KEYS,
  BUILD_REQUIRED_KEYS,
  type ResolvedApp,
  type ResolvedDatabase,
  type ResolvedProjectConfig,
} from "./generated.js";

export { CONFIG_FILENAME, CONFIG_ENV_VAR, CLI_READ_FIELDS, DEFAULTS };
export type { ResolvedApp, ResolvedDatabase, ResolvedProjectConfig };

/**
 * Printed with every environment-shape refusal, because the rule is the point
 * and the missing key is only how it was broken.
 */
const NON_INHERITABLE_NOTE =
  "`apps`, `control` and `databases` are NON-INHERITABLE, and each map must cover every " +
  "label the root declares. An environment that names a control and inherits the root app " +
  "targets the wrong code; one that inherits a database id lands WRITES in the wrong data.";

/** The databases a resolved config declares. Absent means none, not unknown. */
export function declaredDatabases(
  config: ResolvedProjectConfig,
): Record<string, ResolvedDatabase> {
  return config.databases ?? {};
}

/** The apps a resolved config declares. Absent means none, not unknown. */
export function declaredApps(config: ResolvedProjectConfig): Record<string, ResolvedApp> {
  return config.apps ?? {};
}

/** One database of the app being built, with its label dereferenced. */
export interface TargetDatabase extends ResolvedDatabase {
  /** The LOCAL label. It reaches the runtime only through the manifest. */
  label: string;
  /** Whether this is the app's `env.db`. Exactly one target carries it. */
  primary: boolean;
}

/** Which declared app this build is for, and the databases it uses. */
export interface BuildTarget {
  /** `null` only when no file declares any app: a scratch directory. */
  label: string | null;
  appId?: string;
  databases: TargetDatabase[];
}

/**
 * Choose the app a build targets, the way the CLI chooses the one a command
 * targets: the label names an entry of this file, a workspace declaring one
 * app implies it, and a workspace declaring several must be told which.
 */
export function selectBuildTarget(
  config: ResolvedProjectConfig,
  appLabel?: string,
): BuildTarget {
  const apps = declaredApps(config);
  const declared = Object.keys(apps);
  if (appLabel == null && declared.length === 0) {
    return { label: null, databases: [] };
  }
  let label: string;
  if (appLabel != null) {
    if (!declared.includes(appLabel)) {
      throw new Error(
        `zeroship: app "${appLabel}" is not declared in ${CONFIG_FILENAME} ` +
          `(declared: ${joinOrNone(declared)})`,
      );
    }
    label = appLabel;
  } else if (declared.length === 1) {
    label = declared[0]!;
  } else {
    throw new Error(
      `zeroship: ${CONFIG_FILENAME} declares more than one app (${declared.join(", ")}). ` +
        `Pass \`zeroship({ app: "<label>" })\` to say which one this build is.`,
    );
  }

  const entry = apps[label]!;
  const known = declaredDatabases(config);
  const databases = (entry.databases ?? []).map((name) => {
    const database = known[name];
    if (database == null) {
      throw new Error(
        `zeroship: \`apps.${label}.databases\` names "${name}", which ${CONFIG_FILENAME} does ` +
          `not declare under \`databases\``,
      );
    }
    return { ...database, label: name, primary: entry.primary === name };
  });
  return { label, appId: entry.app, databases };
}

/**
 * The database whose gen-types directory is `outDir`, as the emitter needs it.
 *
 * A workspace has one artifact directory per DATABASE (`databases.<label>.out`,
 * which `checkDatabaseOutputs` refuses to share), so a directory identifies a
 * database. `undefined` when no declaration claims it: the caller decides
 * whether that is a refusal or a skip.
 *
 * `primary` is a WORKSPACE fact here, not a per-app one, because that single
 * directory holds the single `env.db.ts` every app using the database is typed
 * from. `checkPrimacyIsUniform` refuses a workspace whose apps disagree, so
 * "some app names it primary" and "every app using it names it primary" are
 * the same answer.
 */
export function databaseForOutDir(
  config: ResolvedProjectConfig,
  root: string,
  outDir: string,
): TargetDatabase | undefined {
  const target = resolve(outDir);
  const found = Object.entries(declaredDatabases(config)).find(
    ([, declared]) => resolve(root, declared.out) === target,
  );
  if (found == null) return undefined;
  const [label, declared] = found;
  const primary = Object.values(declaredApps(config)).some((app) => app.primary === label);
  return { ...declared, label, primary };
}

/**
 * Which database a single-database tool addresses: the one named, or the app's
 * primary. `undefined` when the app declares none, which is a schema-less app
 * rather than an error.
 */
export function selectDatabase(
  config: ResolvedProjectConfig,
  opts: { app?: string; database?: string } = {},
): TargetDatabase | undefined {
  const target = selectBuildTarget(config, opts.app);
  if (opts.database == null) return target.databases.find((d) => d.primary);
  const found = target.databases.find((d) => d.label === opts.database);
  if (found == null) {
    throw new Error(
      `zeroship: database "${opts.database}" is not one of the databases ` +
        `\`apps.${target.label}\` uses ` +
        `(${joinOrNone(target.databases.map((d) => d.label))})`,
    );
  }
  return found;
}

type Json = Record<string, unknown>;

/**
 * The `config` escape hatch: a partial object shallow-merged
 * over the loaded file, or a function applied after the file loads and after
 * environment selection.
 */
export type ProjectConfigOverride =
  | Partial<ResolvedProjectConfig>
  | ((resolved: ResolvedProjectConfig) => Partial<ResolvedProjectConfig>);

// ---------------------------------------------------------------------------
// Locating
// ---------------------------------------------------------------------------

/**
 * `configPath` option, then `ZEROSHIP_CONFIG`, then `zeroship.jsonc` in the app
 * root - and nothing else.
 *
 * NO FORMAT FALLBACKS: Cloudflare's `.jsonc` -> `.json` -> `.toml` search is a
 * back-compat artifact and pre-launch has no legacy files to accept.
 *
 * NO UPWARD DIRECTORY WALK: a build run in a subdirectory would silently pick
 * up a sibling app's `app` and `control`, which is the cross-targeting hazard
 * the environments rule exists to close, arriving through the file-location
 * door.
 *
 * An explicitly named file that does not exist THROWS. Only auto-discovery is
 * allowed to come up empty, because that is the `zeroship()`-in-a-scratch-
 * directory case the plugin must keep serving.
 */
export function locateProjectConfig(root: string, configPath?: string): string | null {
  if (configPath != null) {
    const p = isAbsolute(configPath) ? configPath : resolve(root, configPath);
    if (!existsSync(p)) {
      throw new Error(`zeroship: configPath "${configPath}" does not exist (looked at ${p})`);
    }
    return p;
  }
  const fromEnv = process.env[CONFIG_ENV_VAR];
  if (fromEnv != null && fromEnv !== "") {
    const p = isAbsolute(fromEnv) ? fromEnv : resolve(root, fromEnv);
    if (!existsSync(p)) {
      throw new Error(`zeroship: ${CONFIG_ENV_VAR}="${fromEnv}" does not exist (looked at ${p})`);
    }
    return p;
  }
  const auto = resolve(root, CONFIG_FILENAME);
  return existsSync(auto) ? auto : null;
}

// ---------------------------------------------------------------------------
// Parsing + validation
// ---------------------------------------------------------------------------

export interface LoadedProjectConfig {
  path: string;
  raw: Json;
}

function findUnpairedSurrogate(node: Node): number | null {
  if (
    node.type === "string" &&
    typeof node.value === "string" &&
    /[\uD800-\uDFFF]/u.test(node.value)
  ) {
    return node.offset;
  }
  for (const child of node.children ?? []) {
    const found = findUnpairedSurrogate(child);
    if (found != null) return found;
  }
  return null;
}

export function parseProjectConfig(path: string, text: string): LoadedProjectConfig {
  const bareCarriageReturn = text.search(/\r(?!\n)/);
  if (bareCarriageReturn >= 0) {
    throw new Error(`${path}: bare carriage return in JSONC at offset ${bareCarriageReturn}`);
  }
  const errors: ParseError[] = [];
  const tree = parseTree(text, errors, { allowTrailingComma: true });
  if (errors.length > 0) {
    const error = errors[0];
    throw new Error(
      `${path}: ${printParseErrorCode(error.error)} at offset ${error.offset}`,
    );
  }
  if (tree != null) {
    const unpairedSurrogate = findUnpairedSurrogate(tree);
    if (unpairedSurrogate != null) {
      throw new Error(`${path}: unpaired surrogate at offset ${unpairedSurrogate}`);
    }
  }
  const raw: unknown = tree == null ? undefined : getNodeValue(tree);
  if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error(`${path}: the top level must be an object`);
  }
  validate(path, raw as Json);
  return { path, raw: raw as Json };
}

export function loadProjectConfig(path: string): LoadedProjectConfig {
  return parseProjectConfig(path, readFileSync(path, "utf8"));
}

function validate(path: string, root: Json): void {
  rejectForbiddenNames(path, root, "");
  // A `$schema` naming a different contract is a file written against other
  // rules. Validating it against v1 would report the mismatches as the
  // creator's typos. The Rust reader refuses the same thing; an asymmetry here
  // would mean one tool loading a file the other rejects, which is the class of
  // divergence this whole file exists to remove.
  if (typeof root.$schema === "string" && root.$schema !== SCHEMA_ID) {
    throw new Error(`${path}: $schema is ${root.$schema}, but this build reads ${SCHEMA_ID}`);
  }
  checkObject(path, root, "", ROOT_KNOWN_KEYS, ROOT_REQUIRED_KEYS);
  checkMembers(path, root, "", true);
  checkAppWiring(path, root);

  const envs = root.environments;
  if (envs != null) {
    if (typeof envs !== "object" || Array.isArray(envs)) {
      throw new Error(`${path}: environments must be an object of named targets`);
    }
    for (const [name, entry] of Object.entries(envs as Json)) {
      if (entry == null || typeof entry !== "object" || Array.isArray(entry)) {
        throw new Error(`${path}: environments.${name} must be an object`);
      }
      try {
        checkObject(path, entry as Json, `environments.${name}`, ENVIRONMENT_KNOWN_KEYS, ENVIRONMENT_REQUIRED_KEYS);
      } catch (e) {
        throw new Error(`${(e as Error).message}\n${NON_INHERITABLE_NOTE}`);
      }
      checkMembers(path, entry as Json, `environments.${name}`, false);
      checkEnvironmentLabels(path, root, name, entry as Json);
    }
  }
}

/** The labels one map declares, in file order. */
function labelsOf(map: Json, section: string): string[] {
  const entries = map[section];
  if (entries == null || typeof entries !== "object" || Array.isArray(entries)) return [];
  return Object.keys(entries as Json);
}

function joinOrNone(names: readonly string[]): string {
  return names.length ? names.join(", ") : "none";
}

/**
 * An app names database LABELS, so every one has to resolve here, and the
 * primary has to be one of them. Declaring a database does not grant access to
 * it: `zeroship db bind` does, and deploy verifies the binding.
 */
function checkAppWiring(path: string, root: Json): void {
  const declared = labelsOf(root, "databases");
  // Which apps make each database their `env.db`, and which merely use it.
  // Collected across the whole file because ONE database has ONE gen-types
  // directory - see the refusal below.
  const primaryOf = new Map<string, string>();
  const secondaryOf = new Map<string, string>();
  for (const [label, raw] of Object.entries((root.apps ?? {}) as Json)) {
    const entry = raw as Json;
    const used = (entry.databases ?? []) as string[];
    used.forEach((name, index) => {
      if (!declared.includes(name)) {
        throw new Error(
          `${path}: \`apps.${label}.databases\` names \`${name}\`, which this file does not ` +
            `declare under \`databases\` (declared: ${joinOrNone(declared)})`,
        );
      }
      if (used.slice(0, index).includes(name)) {
        throw new Error(`${path}: \`apps.${label}.databases\` names \`${name}\` twice`);
      }
    });
    const primary = entry.primary as string | undefined;
    if (primary != null && used.length === 0) {
      throw new Error(
        `${path}: \`apps.${label}.primary\` is \`${primary}\`, but \`apps.${label}.databases\` ` +
          `is empty. An app with no database has no primary and no \`env.db\`.`,
      );
    }
    if (primary != null && !used.includes(primary)) {
      throw new Error(
        `${path}: \`apps.${label}.primary\` is \`${primary}\`, which is not one of ` +
          `\`apps.${label}.databases\` (${joinOrNone(used)})`,
      );
    }
    if (primary == null && used.length > 0) {
      throw new Error(
        `${path}: \`apps.${label}\` uses ${joinOrNone(used)} but names no \`primary\`. The ` +
          `primary is \`env.db\`, and \`env.db === env.databases[primary]\` by object identity, ` +
          `so it cannot be inferred.`,
      );
    }
    for (const name of used) {
      const bucket = name === primary ? primaryOf : secondaryOf;
      if (!bucket.has(name)) bucket.set(name, label);
    }
  }
  checkPrimacyIsUniform(path, primaryOf, secondaryOf);
}

/**
 * A database is the `env.db` of every app that uses it, or of none of them.
 *
 * ONE DATABASE HAS ONE GEN-TYPES DIRECTORY (`databases.<label>.out`, which
 * `checkDatabaseOutputs` already refuses to share), so it has ONE `env.db.ts`.
 * That module declares the database's entry on `EnvDatabases` under its label
 * always, and declares `Env.db` only when it is the primary. A workspace where
 * one app makes `analytics` its primary and another merely uses it is asking
 * that single file to be two different files: whichever app built last wins,
 * and the other's `env.db` is typed as the wrong database or not at all.
 *
 * Refused here rather than discovered as a drift-gate failure, because a
 * `--check` complaining that `env.db.ts` drifted names the artifact and not
 * the two lines of configuration that cannot both hold.
 */
function checkPrimacyIsUniform(
  path: string,
  primaryOf: ReadonlyMap<string, string>,
  secondaryOf: ReadonlyMap<string, string>,
): void {
  for (const [database, app] of primaryOf) {
    const other = secondaryOf.get(database);
    if (other == null) continue;
    throw new Error(
      `${path}: \`apps.${app}\` makes \`${database}\` its \`primary\` while \`apps.${other}\` ` +
        `uses it without naming it. A database has ONE generated \`env.db.ts\` ` +
        `(\`databases.${database}.out\`), and that file declares \`Env.db\` only for a ` +
        `primary, so the two apps cannot both be typed from it. Give one of them its own ` +
        `database, or make \`${database}\` the primary of both.`,
    );
  }
}

/**
 * An environment's `apps` and `databases` must cover every label the root
 * declares, and no others.
 *
 * Partial coverage is the whole hazard: an environment that names a production
 * control and inherits a development database id lands WRITES in the wrong
 * place. Requiring the key rather than the whole entry is what keeps the LABEL
 * local - an environment overrides an id, never a label.
 */
function checkEnvironmentLabels(path: string, root: Json, name: string, entry: Json): void {
  for (const section of ["apps", "databases"] as const) {
    const declared = labelsOf(root, section);
    const overridden = labelsOf(entry, section);
    for (const label of declared) {
      if (!overridden.includes(label)) {
        throw new Error(
          `${path}: \`environments.${name}.${section}\` does not name \`${label}\`, which the ` +
            `root declares.\n${NON_INHERITABLE_NOTE}`,
        );
      }
    }
    for (const label of overridden) {
      if (!declared.includes(label)) {
        throw new Error(
          `${path}: \`environments.${name}.${section}.${label}\` names no root \`${section}\` ` +
            `entry (declared: ${joinOrNone(declared)}). An environment overrides the id under a ` +
            `label, never the label itself.`,
        );
      }
    }
  }
}

function rejectForbiddenNames(path: string, value: unknown, at: string): void {
  if (Array.isArray(value)) {
    value.forEach((v, i) => rejectForbiddenNames(path, v, `${at}[${i}]`));
    return;
  }
  if (value == null || typeof value !== "object") return;
  for (const [k, v] of Object.entries(value as Json)) {
    if (FORBIDDEN_KEY_NAMES.includes(k)) {
      const where = at === "" ? k : `${at}.${k}`;
      throw new Error(
        `${path}: \`${where}\` may not appear in this file - it is tracked, and a plaintext ` +
          `secret in a tracked file is unrecoverable once committed. Use ` +
          `\`zeroship secret set ${k.toUpperCase()}=<value> --app=<id>\` for a deployed value, ` +
          `or \`.env\` for a dev one, and declare only the NAME here under \`secrets\`.`,
      );
    }
    rejectForbiddenNames(path, v, at === "" ? k : `${at}.${k}`);
  }
}

function checkObject(
  path: string,
  map: Json,
  at: string,
  known: readonly string[],
  required: readonly string[],
): void {
  const qual = (k: string) => (at === "" ? k : `${at}.${k}`);
  for (const k of Object.keys(map)) {
    if (!known.includes(k)) {
      throw new Error(`${path}: unknown key \`${qual(k)}\` (known: ${known.join(", ")})`);
    }
  }
  for (const k of required) {
    if (!(k in map)) throw new Error(`${path}: missing required key \`${qual(k)}\``);
  }
}

function ruleFor(dotted: string) {
  return FIELD_RULES.find((r) => r.path === dotted);
}

function checkString(path: string, value: unknown, at: string, dotted: string): void {
  if (typeof value !== "string") throw new Error(`${path}: \`${at}\` must be a string`);
  const rule = ruleFor(dotted);
  if (rule?.enum && !rule.enum.includes(value)) {
    throw new Error(`${path}: \`${at}\` must be one of ${rule.enum.join(" | ")} (got \`${value}\`)`);
  }
  if (rule?.pattern && !new RegExp(rule.pattern).test(value)) {
    if (dotted === "build.dist") {
      throw new Error(
        `${path}: \`${at}\` cannot resolve to the project root or an ancestor containing ` +
          `${CONFIG_FILENAME}`,
      );
    }
    throw new Error(`${path}: \`${at}\` must match ${rule.pattern} (got \`${value}\`)`);
  }
}

/** The rule a creator-chosen LABEL must match, by the map it keys. */
function labelRuleFor(mapPath: string) {
  return LABEL_RULES.find((r) => r.path === mapPath);
}

function checkLabel(path: string, label: unknown, at: string, mapPath: string): void {
  const rule = labelRuleFor(mapPath);
  if (typeof label !== "string" || (rule != null && !new RegExp(rule.pattern).test(label))) {
    throw new Error(
      `${path}: \`${at}.${String(label)}\` is not a usable label: it must match ` +
        `${rule?.pattern}. A label is a member name on \`env.databases\` as well as a key here.`,
    );
  }
}

/** Read one label map, checking every key against the rule the schema states. */
function checkLabelMap(
  path: string,
  value: unknown,
  at: string,
  mapPath: string,
): [string, Json][] {
  if (value == null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${path}: \`${at}\` must be an object keyed by LOCAL LABELS`);
  }
  return Object.entries(value as Json).map(([label, entry]) => {
    checkLabel(path, label, at, mapPath);
    if (entry == null || typeof entry !== "object" || Array.isArray(entry)) {
      throw new Error(`${path}: \`${at}.${label}\` must be an object`);
    }
    return [label, entry as Json];
  });
}

function pathContains(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === "" || (rel !== ".." && !rel.startsWith(`..${sep}`) && !isAbsolute(rel));
}

function resolveExistingPath(path: string): string {
  const missing: string[] = [];
  let cursor = resolve(path);
  while (true) {
    try {
      return resolve(realpathSync(cursor), ...missing.reverse());
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      const parent = dirname(cursor);
      if (parent === cursor) throw error;
      missing.push(basename(cursor));
      cursor = parent;
    }
  }
}

export function resolvedPathContains(parent: string, child: string): boolean {
  return pathContains(resolveExistingPath(parent), resolveExistingPath(child));
}

function assertWritablePathIsSafe(
  root: string,
  configPath: string,
  field: string,
  configuredPath: string,
  existingArtifactExtension?: string,
): void {
  const candidate = resolve(root, configuredPath);
  const absoluteConfig = resolve(configPath);
  if (resolvedPathContains(candidate, root) || resolvedPathContains(candidate, absoluteConfig)) {
    throw new Error(
      `${configPath}: \`${field}\` (${configuredPath}) cannot resolve to the project root or ` +
        `an ancestor containing ${CONFIG_FILENAME}`,
    );
  }
  if (existingArtifactExtension == null) return;

  let stat: ReturnType<typeof lstatSync>;
  try {
    stat = lstatSync(candidate);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
    throw error;
  }
  if (!stat.isFile() || stat.isSymbolicLink() || extname(candidate) !== existingArtifactExtension) {
    throw new Error(
      `${configPath}: \`${field}\` (${configuredPath}) resolves to existing non-artifact file ` +
        `${candidate}; refusing to overwrite creator data`,
    );
  }
}

function assertWritablePathsAreSafe(
  root: string,
  configPath: string,
  config: ResolvedProjectConfig,
): void {
  assertWritablePathIsSafe(root, configPath, "build.dist", config.build.dist);
  assertWritablePathIsSafe(root, configPath, "build.output", config.build.output, ".zship");
  // Every database's gen-types directory is a real write target, and no two
  // may share one: `env.db.ts`, `schema.runtime.json` and `migrations.ir.json`
  // have fixed names, so a shared directory is one database's schema silently
  // standing in for another's.
  const claimed = new Map<string, string>();
  for (const [label, database] of Object.entries(declaredDatabases(config))) {
    assertWritablePathIsSafe(root, configPath, `databases.${label}.out`, database.out);
    const resolved = resolve(root, database.out);
    const other = claimed.get(resolved);
    if (other != null) {
      throw new Error(
        `${configPath}: \`databases.${label}.out\` and \`databases.${other}.out\` both resolve ` +
          `to ${resolved}. Two databases cannot share one gen-types directory: the filenames ` +
          `in it are fixed, so one database's schema would overwrite the other's.`,
      );
    }
    claimed.set(resolved, label);
  }
}

function checkMembers(path: string, map: Json, at: string, isRoot: boolean): void {
  const qual = (k: string) => (at === "" ? k : `${at}.${k}`);
  for (const [k, v] of Object.entries(map)) {
    switch (k) {
      case "$schema":
      case "environments":
        break;
      case "protected":
        if (typeof v !== "boolean") throw new Error(`${path}: \`${qual(k)}\` must be a boolean`);
        break;
      case "secrets": {
        if (!Array.isArray(v)) {
          throw new Error(`${path}: \`${qual(k)}\` must be an array of secret NAMES`);
        }
        const rule = ruleFor("secrets");
        for (const item of v) {
          if (typeof item !== "string" || (rule?.itemPattern && !new RegExp(rule.itemPattern).test(item))) {
            throw new Error(
              `${path}: \`${qual(k)}\` entry \`${String(item)}\` must match ${rule?.itemPattern} - ` +
                `this array holds NAMES, never values`,
            );
          }
        }
        break;
      }
      case "build": {
        if (v == null || typeof v !== "object" || Array.isArray(v)) {
          throw new Error(`${path}: \`${qual(k)}\` must be an object`);
        }
        checkObject(path, v as Json, qual(k), BUILD_KNOWN_KEYS, isRoot ? BUILD_REQUIRED_KEYS : []);
        for (const [mk, mv] of Object.entries(v as Json)) {
          checkString(path, mv, `${qual(k)}.${mk}`, `${k}.${mk}`);
        }
        break;
      }
      case "databases": {
        const known = isRoot ? DATABASE_KNOWN_KEYS : ENVIRONMENT_DATABASE_KNOWN_KEYS;
        const required = isRoot ? DATABASE_REQUIRED_KEYS : ENVIRONMENT_DATABASE_REQUIRED_KEYS;
        for (const [label, entry] of checkLabelMap(path, v, qual(k), "databases.*")) {
          checkObject(path, entry, `${qual(k)}.${label}`, known, required);
          for (const [mk, mv] of Object.entries(entry)) {
            checkString(path, mv, `${qual(k)}.${label}.${mk}`, `databases.*.${mk}`);
          }
        }
        break;
      }
      case "apps": {
        const known = isRoot ? APP_KNOWN_KEYS : ENVIRONMENT_APP_KNOWN_KEYS;
        const required = isRoot ? APP_REQUIRED_KEYS : ENVIRONMENT_APP_REQUIRED_KEYS;
        for (const [label, entry] of checkLabelMap(path, v, qual(k), "apps.*")) {
          checkObject(path, entry, `${qual(k)}.${label}`, known, required);
          for (const [mk, mv] of Object.entries(entry)) {
            if (mk === "databases") {
              if (!Array.isArray(mv)) {
                throw new Error(
                  `${path}: \`${qual(k)}.${label}.databases\` must be an array of database LABELS`,
                );
              }
              for (const used of mv) {
                checkLabel(path, used, `${qual(k)}.${label}.databases`, "databases.*");
              }
              continue;
            }
            checkString(path, mv, `${qual(k)}.${label}.${mk}`, `apps.*.${mk}`);
          }
        }
        break;
      }
      default:
        checkString(path, v, qual(k), k);
    }
  }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

function deepMergeBlock(base: unknown, over: unknown): unknown {
  if (
    base != null && typeof base === "object" && !Array.isArray(base) &&
    over != null && typeof over === "object" && !Array.isArray(over)
  ) {
    return { ...(base as Json), ...(over as Json) };
  }
  return over;
}

/**
 * Overlay one label map, entry by entry and member by member.
 *
 * `apps` and `databases` merge one level deeper than anything else because an
 * environment overrides the ID under a label, never the label and never the
 * build-time paths beside it. A whole-entry replace would drop `migrations`,
 * `out`, `databases` and `primary` on the floor.
 */
function mergeLabelMap(base: unknown, over: unknown): unknown {
  if (base == null || typeof base !== "object" || over == null || typeof over !== "object") {
    return over;
  }
  const merged: Json = { ...(base as Json) };
  for (const [label, overridden] of Object.entries(over as Json)) {
    merged[label] = deepMergeBlock(merged[label], overridden);
  }
  return merged;
}

/**
 * The file's own facts, with the named environment overlaid and every schema
 * default applied.
 *
 * `apps`, `control` and `databases` come from the environment ALONE when one is
 * selected (the schema requires all three there, and each map must cover every
 * root label); `build` and `secrets` merge member by member over the root.
 */
export function resolveProjectConfig(
  loaded: LoadedProjectConfig,
  environment?: string,
): ResolvedProjectConfig {
  const out: Json = { ...loaded.raw };
  delete out.$schema;
  delete out.environments;

  if (environment != null) {
    const entry = (loaded.raw.environments as Json | undefined)?.[environment] as Json | undefined;
    if (entry == null) {
      const known = Object.keys((loaded.raw.environments as Json | undefined) ?? {});
      throw new Error(
        `${loaded.path}: --env=${environment} names no environment (declared: ${known.length ? known.join(", ") : "none"})`,
      );
    }
    for (const [k, v] of Object.entries(entry)) {
      out[k] =
        k === "apps" || k === "databases"
          ? mergeLabelMap(out[k], v)
          : k === "build"
            ? deepMergeBlock(out[k], v)
            : v;
    }
  }

  return withDefaults(out);
}

/** The plugin's config when there is no file at all: defaults only. */
export function defaultProjectConfig(): ResolvedProjectConfig {
  return withDefaults({});
}

function withDefaults(value: Json): ResolvedProjectConfig {
  const out: Json = { ...value };
  for (const [dotted, def] of Object.entries(DEFAULTS)) {
    const [head, member] = dotted.split(".");
    if (member == null) {
      if (out[head] === undefined) out[head] = structuredClone(def);
      continue;
    }
    const block = { ...((out[head] as Json | undefined) ?? {}) };
    if (block[member] === undefined) block[member] = structuredClone(def);
    out[head] = block;
  }
  return out as unknown as ResolvedProjectConfig;
}

const FILE_RELATIVE_PATHS = [
  ["build", "serverEntry"],
  ["build", "dist"],
  ["build", "output"],
] as const;

/** The per-database members that name a directory relative to the file. */
const DATABASE_RELATIVE_PATHS = ["migrations", "out"] as const;

/** Root paths stated by a config file at that file's own directory. */
function rootFilePaths(
  config: ResolvedProjectConfig,
  configRoot: string,
): ResolvedProjectConfig {
  const out = structuredClone(config) as unknown as Json;
  const reroot = (block: Json, member: string) => {
    const value = block[member];
    if (typeof value === "string" && !isAbsolute(value)) {
      block[member] = resolve(configRoot, value);
    }
  };
  for (const [section, member] of FILE_RELATIVE_PATHS) {
    const block = out[section] as Json | undefined;
    if (block != null) reroot(block, member);
  }
  for (const database of Object.values((out.databases ?? {}) as Json)) {
    for (const member of DATABASE_RELATIVE_PATHS) reroot(database as Json, member);
  }
  return out as unknown as ResolvedProjectConfig;
}

// ---------------------------------------------------------------------------
// The `config` escape hatch
// ---------------------------------------------------------------------------

/**
 * Project one dotted path, expanding a `*` segment over every label present.
 *
 * The wildcard is what makes the deny-list below cover a map: without it
 * `databases.*.id` would read `undefined` on both sides and every id change
 * would compare equal. Expanding to a keyed object also catches a label added
 * or removed rather than only one edited.
 */
function at(value: unknown, dotted: string): unknown {
  return project(value, dotted.split("."));
}

function project(cursor: unknown, segments: readonly string[]): unknown {
  if (segments.length === 0) return cursor;
  if (cursor == null || typeof cursor !== "object") return undefined;
  const [head, ...rest] = segments;
  if (head !== "*") return project((cursor as Json)[head!], rest);
  const out: Json = {};
  for (const key of Object.keys(cursor as Json).sort()) {
    out[key] = project((cursor as Json)[key], rest);
  }
  return out;
}

/**
 * Apply the `config` escape hatch, refusing any CHANGE to a field the Rust CLI
 * also reads.
 *
 * DENIAL IS ON CHANGE, NOT PRESENCE, and that is forced by the idiom: the
 * documented shape is `(c) => ({ ...c, build: { ...c.build, mode: ... } })`,
 * which spreads `app` and `control` into the result every single time. A
 * presence check would reject the only form anybody writes.
 *
 * WHY THE RESTRICTION EXISTS AT ALL. A `config` function runs inside Vite. The
 * Rust CLI cannot execute it and never will - it is the same wall that produced
 * the original defect. Letting the hatch move a CLI-read field would
 * reintroduce the exact drift this file removes, wearing a feature's clothing.
 * The deny-list is GENERATED from the schema's `x-cli-read` markers, so it
 * cannot fall behind the fact it protects.
 */
export function applyProjectConfigOverride(
  resolved: ResolvedProjectConfig,
  override: ProjectConfigOverride | undefined,
): ResolvedProjectConfig {
  if (override == null) return resolved;
  const baseline = structuredClone(resolved);
  const working = structuredClone(resolved);
  const partial = typeof override === "function" ? override(working) : override;
  const next: Json = { ...(working as unknown as Json) };
  for (const [k, v] of Object.entries(partial as Json)) {
    next[k] =
      k === "apps" || k === "databases"
        ? mergeLabelMap(next[k], v)
        : k === "build"
          ? deepMergeBlock(next[k], v)
          : v;
  }

  for (const field of CLI_READ_FIELDS) {
    const before = at(baseline, field);
    const after = at(next, field);
    if (JSON.stringify(before) !== JSON.stringify(after)) {
      throw new Error(
        `zeroship: the \`config\` option may not change \`${field}\` (tried ${JSON.stringify(before)} -> ` +
          `${JSON.stringify(after)}).\n` +
          `That field is read by the \`zeroship\` CLI as well as by the build, and the CLI cannot ` +
          `run a JavaScript function. Overriding it here would make the two disagree - which is the ` +
          `drift ${CONFIG_FILENAME} exists to remove. Change it in ${CONFIG_FILENAME}, or use an ` +
          `\`environments\` entry and \`--env=\`.\n` +
          `Fields the build alone reads (build.mode, build.serverEntry, build.dist) ` +
          `are overridable here.`,
      );
    }
  }
  return next as unknown as ResolvedProjectConfig;
}

// ---------------------------------------------------------------------------
// The canonical dump
// ---------------------------------------------------------------------------

/**
 * Object keys sorted and compact. The package tests pin this reader's output;
 * the CLI tests pin the Rust reader against the same generated schema and
 * committed fixture.
 */
export function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value != null && typeof value === "object") {
    const keys = Object.keys(value as Json).sort();
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalJson((value as Json)[k])}`).join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

export interface ProjectConfigInput {
  configPath?: string;
  environment?: string;
  override?: ProjectConfigOverride;
}

/**
 * The whole read path in one call, for the plugin and its two scripts.
 *
 * Returns the defaults-only config when no file is found, which is what keeps
 * `zeroship()` working in a scratch directory.
 */
export function readProjectConfig(
  root: string,
  opts: ProjectConfigInput = {},
): { config: ResolvedProjectConfig; path: string | null } {
  const path = locateProjectConfig(root, opts.configPath);
  const configRoot = path == null ? resolve(root) : dirname(path);
  const base = path == null
    ? defaultProjectConfig()
    : resolveProjectConfig(loadProjectConfig(path), opts.environment);
  const overridden = applyProjectConfigOverride(base, opts.override);
  const config = path == null ? overridden : rootFilePaths(overridden, configRoot);
  assertWritablePathsAreSafe(configRoot, path ?? resolve(root, CONFIG_FILENAME), config);
  return { config, path };
}

/**
 * A memoised reader shared by every plugin in one `zeroship()` call.
 *
 * The Vite root is not known when `zeroship()` runs (it arrives in `config` /
 * `configResolved`), so the read has to be lazy. Memoising it per root is what
 * stops the build plugin and the dev-server plugin from reading the file twice
 * and, on a file changed mid-run, disagreeing about what it says.
 */
export interface ProjectConfigHolder {
  load(root: string): ResolvedProjectConfig;
  /** The file that was read, or `null` when none was found. Valid after `load`. */
  path(): string | null;
}

export function createProjectConfigHolder(input: ProjectConfigInput): ProjectConfigHolder {
  const cache = new Map<string, { config: ResolvedProjectConfig; path: string | null }>();
  let last: string | null = null;
  return {
    load(root: string): ResolvedProjectConfig {
      let hit = cache.get(root);
      if (hit == null) {
        hit = readProjectConfig(root, input);
        cache.set(root, hit);
      }
      last = hit.path;
      return hit.config;
    },
    path(): string | null {
      return last;
    },
  };
}
