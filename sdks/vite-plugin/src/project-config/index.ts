/**
 * `zeroship.jsonc` - the creator project configuration, build side.
 *
 * SCOPE INVARIANT: this file is
 * read by the `zeroship` CLI and by the build toolchain. It is NEVER read by
 * the runtime and NEVER packed into a `.zship`. The packer walks `distDir` and
 * `zeroship.jsonc` lives one level above it, so the invariant holds by
 * construction today - which is exactly why `tests/project_config_gate.sh`
 * asserts it on the ARCHIVE BYTES rather than trusting the construction.
 *
 * THIS IS THE SIDE THAT HAS DEFAULTS. The plugin must work with
 * `zeroship()` and no file at all - that is what the scaffold ships - so every
 * schema `default` is applied here. The Rust CLI has none: a key it reads and
 * the file omits is an error naming the key. One default, one holder, no way
 * for the two readers to disagree.
 */

import { existsSync, readFileSync } from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";

import { stripJsonc } from "./jsonc.js";
import {
  CLI_READ_FIELDS,
  CONFIG_ENV_VAR,
  CONFIG_FILENAME,
  DEFAULTS,
  ENVIRONMENT_KNOWN_KEYS,
  ENVIRONMENT_REQUIRED_KEYS,
  FIELD_RULES,
  FORBIDDEN_KEY_NAMES,
  ROOT_KNOWN_KEYS,
  SCHEMA_ID,
  ROOT_REQUIRED_KEYS,
  BUILD_KNOWN_KEYS,
  BUILD_REQUIRED_KEYS,
  MIGRATIONS_KNOWN_KEYS,
  MIGRATIONS_REQUIRED_KEYS,
  type ResolvedProjectConfig,
} from "./generated.js";

export { CONFIG_FILENAME, CONFIG_ENV_VAR, CLI_READ_FIELDS, DEFAULTS };
export type { ResolvedProjectConfig };

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

export function parseProjectConfig(path: string, text: string): LoadedProjectConfig {
  let raw: unknown;
  try {
    raw = JSON.parse(stripJsonc(text));
  } catch (e) {
    throw new Error(`${path}: ${(e as Error).message}`);
  }
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
        throw new Error(
          `${(e as Error).message}\n` +
            "`app` and `control` are NON-INHERITABLE: an environment that names a control and " +
            "inherits the root app is exactly the silent cross-targeting this rule exists to prevent.",
        );
      }
      checkMembers(path, entry as Json, `environments.${name}`, false);
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

function pathContains(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === "" || (rel !== ".." && !rel.startsWith(`..${sep}`) && !isAbsolute(rel));
}

function assertDistDoesNotContainConfig(
  root: string,
  configPath: string,
  config: ResolvedProjectConfig,
): void {
  const distDir = resolve(root, config.build.dist);
  const absoluteConfig = resolve(configPath);
  if (pathContains(distDir, resolve(root)) || pathContains(distDir, absoluteConfig)) {
    throw new Error(
      `${configPath}: \`build.dist\` (${config.build.dist}) cannot resolve to the project root or ` +
        `an ancestor containing ${CONFIG_FILENAME}`,
    );
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
      case "build":
      case "migrations": {
        if (v == null || typeof v !== "object" || Array.isArray(v)) {
          throw new Error(`${path}: \`${qual(k)}\` must be an object`);
        }
        const known = k === "build" ? BUILD_KNOWN_KEYS : MIGRATIONS_KNOWN_KEYS;
        const required = isRoot ? (k === "build" ? BUILD_REQUIRED_KEYS : MIGRATIONS_REQUIRED_KEYS) : [];
        checkObject(path, v as Json, qual(k), known, required);
        for (const [mk, mv] of Object.entries(v as Json)) {
          checkString(path, mv, `${qual(k)}.${mk}`, `${k}.${mk}`);
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
 * The file's own facts, with the named environment overlaid and every schema
 * default applied.
 *
 * `app` / `control` come from the environment ALONE when one is selected (the
 * schema requires both there); `build` / `migrations` / `secrets` merge member
 * by member over the root.
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
      out[k] = k === "build" || k === "migrations" ? deepMergeBlock(out[k], v) : v;
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

// ---------------------------------------------------------------------------
// The `config` escape hatch
// ---------------------------------------------------------------------------

function at(value: unknown, dotted: string): unknown {
  let cursor: unknown = value;
  for (const seg of dotted.split(".")) {
    if (cursor == null || typeof cursor !== "object") return undefined;
    cursor = (cursor as Json)[seg];
  }
  return cursor;
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
  const partial = typeof override === "function" ? override(resolved) : override;
  const next: Json = { ...(resolved as unknown as Json) };
  for (const [k, v] of Object.entries(partial as Json)) {
    next[k] = k === "build" || k === "migrations" ? deepMergeBlock(next[k], v) : v;
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
          `Fields the build alone reads (build.mode, build.serverEntry, build.dist, secrets) ` +
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
 * Object keys sorted, compact. Byte-compared against the Rust reader's dump by
 * `tests/project_config_gate.sh` - one comparison that catches divergent
 * defaults, silently ignored keys and type-coercion differences at once.
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
  const base =
    path == null ? defaultProjectConfig() : resolveProjectConfig(loadProjectConfig(path), opts.environment);
  if (path != null) assertDistDoesNotContainConfig(root, path, base);
  return { config: applyProjectConfigOverride(base, opts.override), path };
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
