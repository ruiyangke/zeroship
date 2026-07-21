import { readFileSync, statSync } from "node:fs";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";

const CONFIG_FILENAME = "zero-migrate.toml";

/** Values accepted in one `[env.<name>]` config block. */
export interface ZeroMigrateEnvironmentConfig {
  url?: string;
  dir?: string;
  ownerApp?: string;
  schema?: string;
  registry?: string;
  policy?: readonly string[];
}

/** A discovered and selected config block. */
export interface LoadedZeroMigrateConfig {
  path: string;
  environment: string;
  values: ZeroMigrateEnvironmentConfig;
}

/** CLI-shaped values. `undefined` means that the corresponding flag was absent. */
export interface CliConfigValues {
  databaseUrl?: string;
  dir?: string;
  ownerApp?: string;
  projectSchema?: string;
  registryPath?: string;
  policyPaths?: readonly string[];
}

/** Fully resolved values consumed by the command implementations. */
export interface ResolvedCliConfig {
  databaseUrl?: string;
  dir: string;
  ownerApp: string;
  projectSchema: string;
  registryPath?: string;
  policyPaths: string[];
  /** Present only when a config file was loaded. */
  configPath?: string;
  /** Present only when a config file was loaded. */
  environment?: string;
  /**
   * Non-fatal diagnostics the caller should surface (e.g. an env var that
   * overrode a committed multi-layer policy). Empty when there is nothing to
   * warn about.
   */
  warnings: string[];
}

export interface LoadZeroMigrateConfigOptions {
  /** Directory from which upward discovery starts. Defaults to `process.cwd()`. */
  cwd?: string;
  /** Explicit `--config` path. Relative paths are resolved from `cwd`. */
  configPath?: string;
  /** Explicit `--env` selection. Defaults to `dev`, or the sole block. */
  environment?: string;
  /** Environment used by `env:VARNAME` references and setting precedence. */
  processEnv?: NodeJS.ProcessEnv;
}

export interface ResolveCliConfigOptions extends LoadZeroMigrateConfigOptions {
  /** Values supplied by flags. These have the highest precedence. */
  explicit?: CliConfigValues;
  /** Command defaults. Omitted members use the standard CLI defaults. */
  defaults?: CliConfigValues;
}

/** A config failure suitable for the CLI's clean error path. */
export class ZeroMigrateConfigError extends Error {
  override name = "ZeroMigrateConfigError";
}

interface ParsedEnvironmentConfig {
  url?: string;
  dir?: string;
  owner_app?: string;
  schema?: string;
  registry?: string;
  policy?: string | string[];
}

const CONFIG_KEYS = new Set([
  "url",
  "dir",
  "owner_app",
  "schema",
  "registry",
  "policy",
]);

function configError(path: string, message: string, line?: number): ZeroMigrateConfigError {
  const location = line === undefined ? path : `${path}:${line}`;
  return new ZeroMigrateConfigError(`config ${location}: ${message}`);
}

/** Find `zero-migrate.toml` from `cwd` upward, or resolve an explicit path. */
export function discoverZeroMigrateConfig(
  cwd = process.cwd(),
  configPath?: string,
): string | undefined {
  const start = resolve(cwd);
  if (configPath !== undefined) {
    if (configPath.length === 0) {
      throw new ZeroMigrateConfigError("flag --config needs a non-empty file path");
    }
    const path = isAbsolute(configPath) ? configPath : resolve(start, configPath);
    assertRegularFile(path, true);
    return path;
  }

  let directory = start;
  for (;;) {
    const candidate = join(directory, CONFIG_FILENAME);
    if (assertRegularFile(candidate, false)) return candidate;
    const parent = dirname(directory);
    if (parent === directory) return undefined;
    directory = parent;
  }
}

function assertRegularFile(path: string, required: boolean): boolean {
  try {
    const stat = statSync(path);
    if (!stat.isFile()) {
      if (!required) return false;
      throw new ZeroMigrateConfigError(`config path is not a file: ${path}`);
    }
    return true;
  } catch (error) {
    if (error instanceof ZeroMigrateConfigError) throw error;
    const code = (error as NodeJS.ErrnoException).code;
    if (!required && (code === "ENOENT" || code === "ENOTDIR")) return false;
    throw new ZeroMigrateConfigError(`read config file ${path}: ${(error as Error).message}`);
  }
}

/**
 * Discover, parse, and select a config environment. Returns `undefined` when no
 * config exists and no explicit config/environment selector was supplied.
 */
export function loadZeroMigrateConfig(
  options: LoadZeroMigrateConfigOptions = {},
): LoadedZeroMigrateConfig | undefined {
  const processEnv = options.processEnv ?? process.env;
  const configPath = options.configPath;
  const environment = options.environment;
  const path = discoverZeroMigrateConfig(options.cwd, configPath);
  if (path === undefined) {
    if (environment !== undefined) {
      throw new ZeroMigrateConfigError(
        `environment ${JSON.stringify(environment)} was selected, but no ${CONFIG_FILENAME} was found`,
      );
    }
    return undefined;
  }

  let source: string;
  try {
    source = readFileSync(path, "utf8");
  } catch (error) {
    throw new ZeroMigrateConfigError(`read config file ${path}: ${(error as Error).message}`);
  }
  const environments = parseConfig(source, path);
  const names = [...environments.keys()];
  if (names.length === 0) {
    throw configError(path, "expected at least one [env.<name>] block");
  }

  let selected = environment;
  if (selected === undefined) {
    if (environments.has("dev")) selected = "dev";
    else if (names.length === 1) selected = names[0];
    else {
      throw configError(
        path,
        "has multiple environments and no [env.dev] block; select one with --env <name>",
      );
    }
  }
  const raw = environments.get(selected);
  if (raw === undefined) {
    throw configError(path, `environment ${JSON.stringify(selected)} is not defined`);
  }

  const base = dirname(path);
  const get = (field: keyof ParsedEnvironmentConfig): string | undefined => {
    const value = raw[field];
    if (value === undefined || Array.isArray(value)) return undefined;
    return resolveEnvironmentReference(value, processEnv, path, selected!, field);
  };
  const configPolicy = raw.policy;
  const policy =
    configPolicy === undefined
      ? undefined
      : (Array.isArray(configPolicy) ? configPolicy : [configPolicy]).map((value) =>
          resolveConfigPath(
            resolveEnvironmentReference(value, processEnv, path, selected!, "policy"),
            base,
          ),
        );

  return {
    path,
    environment: selected,
    values: {
      url: get("url"),
      dir: optionalConfigPath(get("dir"), base),
      ownerApp: get("owner_app"),
      schema: get("schema"),
      registry: optionalConfigPath(get("registry"), base),
      policy,
    },
  };
}

/**
 * Resolve command settings using: explicit flags, environment variables,
 * selected config values, then defaults. An absent config preserves the legacy
 * flag/environment/default behavior.
 */
export function resolveCliConfig(options: ResolveCliConfigOptions = {}): ResolvedCliConfig {
  const processEnv = options.processEnv ?? process.env;
  const loaded = loadZeroMigrateConfig({
    cwd: options.cwd,
    configPath: options.configPath ?? processEnv.ZERO_MIGRATE_CONFIG,
    environment: options.environment ?? processEnv.ZERO_MIGRATE_ENV,
    processEnv,
  });
  const explicit = options.explicit ?? {};
  const defaults = options.defaults ?? {};
  const config = loaded?.values;

  // `ZERO_MIGRATE_POLICY` carries an ORDERED list of charter layers, delimited
  // by the OS path separator (`:` on POSIX, `;` on Windows) exactly like `PATH`.
  // Wrapping the raw string in a single-element array (the old behaviour) made it
  // impossible to express a multi-layer policy from the environment, so a stray
  // single-valued env var silently COLLAPSED a committed multi-layer policy to one
  // layer. Since layers only narrow, dropping layers widens effective policy.
  const environmentPolicyRaw = defined(processEnv.ZERO_MIGRATE_POLICY);
  const environmentPolicyLayers =
    environmentPolicyRaw === undefined
      ? undefined
      : (() => {
          const layers = environmentPolicyRaw
            .split(delimiter)
            .map((layer) => layer.trim())
            .filter((layer) => layer.length > 0);
          // An env var set to an empty/whitespace value carries no layers; treat
          // it as absent rather than as an empty (no-charter) policy.
          return layers.length > 0 ? layers : undefined;
        })();

  const warnings: string[] = [];
  if (
    explicit.policyPaths === undefined &&
    environmentPolicyLayers !== undefined &&
    config?.policy !== undefined
  ) {
    const envCount = environmentPolicyLayers.length;
    const configCount = config.policy.length;
    warnings.push(
      `ZERO_MIGRATE_POLICY (${envCount} layer${envCount === 1 ? "" : "s"}) ` +
        `overrides the ${configCount}-layer policy in ` +
        `${loaded?.path ?? "the config file"}; only the environment layers are ` +
        `in effect. Policy layers only narrow, so this can widen effective policy.`,
    );
  }

  return {
    warnings,
    databaseUrl:
      explicit.databaseUrl ??
      defined(processEnv.ZERO_MIGRATE_URL) ??
      config?.url ??
      nonEmpty(processEnv.DATABASE_URL) ??
      defaults.databaseUrl,
    dir:
      explicit.dir ??
      defined(processEnv.ZERO_MIGRATE_DIR) ??
      config?.dir ??
      defaults.dir ??
      "./migrations",
    ownerApp:
      explicit.ownerApp ??
      defined(processEnv.ZERO_MIGRATE_OWNER_APP) ??
      config?.ownerApp ??
      defaults.ownerApp ??
      "app_cli",
    projectSchema:
      explicit.projectSchema ??
      defined(processEnv.ZERO_MIGRATE_SCHEMA) ??
      config?.schema ??
      defaults.projectSchema ??
      "public",
    registryPath:
      explicit.registryPath ??
      defined(processEnv.ZERO_MIGRATE_REGISTRY) ??
      config?.registry ??
      defaults.registryPath,
    policyPaths:
      explicit.policyPaths !== undefined
        ? [...explicit.policyPaths]
        : environmentPolicyLayers !== undefined
          ? environmentPolicyLayers
          : config?.policy !== undefined
            ? [...config.policy]
            : [...(defaults.policyPaths ?? [])],
    configPath: loaded?.path,
    environment: loaded?.environment,
  };
}

function defined(value: string | undefined): string | undefined {
  return value === undefined ? undefined : value;
}

function nonEmpty(value: string | undefined): string | undefined {
  return value === undefined || value.length === 0 ? undefined : value;
}

function optionalConfigPath(value: string | undefined, base: string): string | undefined {
  return value === undefined ? undefined : resolveConfigPath(value, base);
}

function resolveConfigPath(value: string, base: string): string {
  return isAbsolute(value) ? value : resolve(base, value);
}

function resolveEnvironmentReference(
  value: string,
  processEnv: NodeJS.ProcessEnv,
  path: string,
  environment: string,
  field: keyof ParsedEnvironmentConfig,
): string {
  if (!value.startsWith("env:")) return value;
  const variable = value.slice(4);
  if (variable.length === 0) {
    throw configError(
      path,
      `[env.${environment}].${field} has an empty env: variable reference`,
    );
  }
  const resolved = processEnv[variable];
  if (resolved === undefined) {
    throw configError(
      path,
      `[env.${environment}].${field} references unset environment variable ${variable}`,
    );
  }
  return resolved;
}

/** Parse the intentionally small zero-migrate config schema from TOML. */
function parseConfig(source: string, path: string): Map<string, ParsedEnvironmentConfig> {
  const environments = new Map<string, ParsedEnvironmentConfig>();
  let current: ParsedEnvironmentConfig | undefined;
  let currentName: string | undefined;
  const lines = source.replace(/^\uFEFF/, "").split(/\r?\n/);

  for (let index = 0; index < lines.length; index++) {
    const lineNumber = index + 1;
    let line = stripTomlComment(lines[index]).trim();
    if (line.length === 0) continue;

    if (line.startsWith("[")) {
      const match = /^\[env\.([A-Za-z0-9_-]+)\]$/.exec(line);
      if (match === null) {
        throw configError(path, "only [env.<name>] tables are supported", lineNumber);
      }
      currentName = match[1];
      if (environments.has(currentName)) {
        throw configError(path, `duplicate [env.${currentName}] block`, lineNumber);
      }
      current = {};
      environments.set(currentName, current);
      continue;
    }

    if (current === undefined || currentName === undefined) {
      throw configError(path, "settings must be inside an [env.<name>] block", lineNumber);
    }
    const equals = findUnquoted(line, "=");
    if (equals === -1) {
      throw configError(path, "expected key = value", lineNumber);
    }
    const key = line.slice(0, equals).trim();
    if (!CONFIG_KEYS.has(key)) {
      throw configError(path, `unknown setting ${JSON.stringify(key)}`, lineNumber);
    }
    if (Object.hasOwn(current, key)) {
      throw configError(path, `duplicate setting ${JSON.stringify(key)}`, lineNumber);
    }

    let valueSource = line.slice(equals + 1).trim();
    if (valueSource.startsWith("[") && bracketDepth(valueSource) > 0) {
      while (++index < lines.length) {
        const continuation = stripTomlComment(lines[index]).trim();
        valueSource += `\n${continuation}`;
        if (bracketDepth(valueSource) === 0) break;
      }
    }
    let value: string | string[];
    try {
      value = parseTomlStringValue(valueSource);
    } catch (error) {
      throw configError(path, (error as Error).message, lineNumber);
    }
    if (key !== "policy" && Array.isArray(value)) {
      throw configError(path, `${key} must be a string`, lineNumber);
    }
    Object.defineProperty(current, key, {
      value,
      enumerable: true,
      configurable: true,
      writable: true,
    });
  }
  return environments;
}

function stripTomlComment(line: string): string {
  let quote: "\"" | "'" | undefined;
  let escaped = false;
  for (let index = 0; index < line.length; index++) {
    const char = line[index];
    if (quote === "\"") {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === quote) quote = undefined;
    } else if (quote === "'") {
      if (char === quote) quote = undefined;
    } else if (char === "\"" || char === "'") {
      quote = char;
    } else if (char === "#") {
      return line.slice(0, index);
    }
  }
  return line;
}

function findUnquoted(source: string, needle: string): number {
  let quote: "\"" | "'" | undefined;
  let escaped = false;
  for (let index = 0; index < source.length; index++) {
    const char = source[index];
    if (quote === "\"") {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === quote) quote = undefined;
    } else if (quote === "'") {
      if (char === quote) quote = undefined;
    } else if (char === "\"" || char === "'") {
      quote = char;
    } else if (char === needle) {
      return index;
    }
  }
  return -1;
}

function bracketDepth(source: string): number {
  let quote: "\"" | "'" | undefined;
  let escaped = false;
  let depth = 0;
  for (const char of source) {
    if (quote === "\"") {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === quote) quote = undefined;
    } else if (quote === "'") {
      if (char === quote) quote = undefined;
    } else if (char === "\"" || char === "'") {
      quote = char;
    } else if (char === "[") {
      depth++;
    } else if (char === "]") {
      depth--;
    }
  }
  return depth;
}

function parseTomlStringValue(source: string): string | string[] {
  let index = 0;
  const skipWhitespace = (): void => {
    while (/\s/.test(source[index] ?? "")) index++;
  };
  const parseString = (): string => {
    skipWhitespace();
    const quote = source[index];
    if (quote !== "\"" && quote !== "'") {
      throw new Error("values must be quoted TOML strings");
    }
    const start = index++;
    if (quote === "'") {
      const end = source.indexOf("'", index);
      if (end === -1) throw new Error("unterminated TOML string");
      index = end + 1;
      return source.slice(start + 1, end);
    }
    let escaped = false;
    while (index < source.length) {
      const char = source[index++];
      if (escaped) {
        escaped = false;
      } else if (char === "\\") {
        escaped = true;
      } else if (char === "\"") {
        const encoded = source.slice(start, index);
        try {
          return JSON.parse(encoded) as string;
        } catch {
          throw new Error("invalid escape in TOML string");
        }
      }
    }
    throw new Error("unterminated TOML string");
  };

  skipWhitespace();
  if (source[index] !== "[") {
    const value = parseString();
    skipWhitespace();
    if (index !== source.length) throw new Error("unexpected content after string value");
    return value;
  }

  index++;
  const values: string[] = [];
  skipWhitespace();
  while (source[index] !== "]") {
    if (index >= source.length) throw new Error("unterminated string array");
    values.push(parseString());
    skipWhitespace();
    if (source[index] === ",") {
      index++;
      skipWhitespace();
      if (source[index] === "]") break;
    } else if (source[index] !== "]") {
      throw new Error("string array entries must be separated by commas");
    }
  }
  index++;
  skipWhitespace();
  if (index !== source.length) throw new Error("unexpected content after string array");
  return values;
}
