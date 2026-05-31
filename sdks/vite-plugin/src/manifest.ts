// sdks/vite-plugin/src/manifest.ts
//
// Compute the manifest's resource tree:
//
//   - resources    flat map of `<key> -> resource` per
//                  `docs/proposals/rpc.md` §7
//   - transformer  always "json" by default
//
// Validation is wired into the runtime dispatch — the synthetic SSR
// entry calls `proc.config.input.parse(args)` before invoking the
// user handler. The manifest never carries JSONSchemas.
//
// Wire identity (from `docs/proposals/rpc.md` §2): a pure function
// of current source.
//
//   1. fn.config.id (explicit) wins.
//   2. Default = <exportName>.
//   3. Collision check across all assigned wireIds — duplicates fail
//      the build with both file paths.
//   4. Production-mode gate: any procedure that landed at step 2 (no
//      explicit id) is a build error in production.
//
// No previous-build alias state. No persistence. The wireId is always
// what the current source says. (The earlier alias system is gone — see
// the §2 update in `docs/proposals/rpc.md`.)
//
// Wireshape: see `docs/proposals/rpc.md` §7.

import { promises as fs } from "node:fs";
import { resolve } from "node:path";

// ── Public types ───────────────────────────────────────────────────────────

/**
 * One discovered procedure handed off from the transform plugin.
 *
 * The default wireId is the bare `exportName`; identity is a pure
 * function of current source. `moduleSlug` is carried for diagnostic
 * messages only — it does NOT influence the wireId.
 */
export interface DiscoveredProcedure {
  /** Absolute path to the source file. */
  filePath: string;
  /** Bare export name as it appears in source. */
  exportName: string;
  /**
   * Slug derived from the file path (e.g. `src/server/todos.ts` →
   * `src-server-todos`). Diagnostic-only; not used in wireId
   * derivation since the default wireId stopped including the file path.
   */
  moduleSlug: string;
  /**
   * Discriminator. Includes `action` (B3 capability-scoped wrapper);
   * the manifest emitter folds `action` → omitted `kind` in the wire
   * output for back-compat with the Rust-side `ProcedureKind` enum
   * (which predates B3 and would reject an unknown value). The
   * gateway's dispatch treats "no kind" as "no method/transport
   * restriction" — matching action semantics.
   */
  kind: "query" | "mutation" | "action" | "stream" | "subscription";
  /** True if the export is an async generator. */
  isStream: boolean;
  /**
   * Per-procedure metadata pulled out of the AST: contents of
   * `<fnName>.config = { ... }`. Loose typing here — the manifest
   * shape validates at write time.
   */
  config?: Record<string, unknown>;
  /**
   * Module-level `$config` values (auth, rate_limit defaults). Same
   * shape as `config`; merged with lower precedence.
   */
  moduleConfig?: Record<string, unknown>;
}

export interface ManifestExtrasInput {
  root: string;
  procedures: DiscoveredProcedure[];
  /** Production: throw on validation errors. Development: warn. */
  mode: "production" | "development";
  /**
   * Override the canonical config path (`src/server/config.ts`). Tests
   * use this; production builds always read the canonical path.
   */
  configPath?: string;
  /** Hook for logging non-fatal warnings. */
  onWarn?: (msg: string) => void;
}

/** Resource entry on the wire — snake_case fields, mirrors §7. */
export type WireResource = Record<string, unknown>;

export interface ManifestExtras {
  resources: Record<string, WireResource>;
  transformer: "superjson" | "json";
  /** Hint for the manifest schema version (1 — the initial published shape). */
  versionHint: 1;
}

// ── camelCase → snake_case rename map ──────────────────────────────────────
//
// The authoring surface uses camelCase; the wire shape uses snake_case
// (matches Rust serde defaults). Resources flow from one to the other
// via this map, applied per top-level field.

const CAMEL_TO_SNAKE: Record<string, string> = {
  rateLimit: "rate_limit",
  maxInputBytes: "max_input_bytes",
  publiclyAccessible: "publicly_accessible",
  csrfOrigins: "csrf_origins",
  // cache.swr / cache.maxAge handled inside the cache subobject below
};

/** Per-field mapper for fields whose VALUES contain nested camelCase keys. */
function renameCacheControlKeys(
  cache: Record<string, unknown>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(cache)) {
    if (k === "maxAge") out.max_age = v;
    else if (k === "swr") out.swr_window = v;
    else if (k === "staleOnError") out.stale_on_error = v;
    else out[k] = v;
  }
  return out;
}

function renameCorsKeys(cors: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(cors)) {
    if (k === "allowOrigins") out.allow_origins = v;
    else if (k === "allowMethods") out.allow_methods = v;
    else if (k === "allowHeaders") out.allow_headers = v;
    else if (k === "exposeHeaders") out.expose_headers = v;
    else if (k === "allowCredentials") out.allow_credentials = v;
    else if (k === "maxAge") out.max_age = v;
    else out[k] = v;
  }
  return out;
}

function renameRedirectKeys(
  red: string | Record<string, unknown>,
): Record<string, unknown> {
  if (typeof red === "string") return { to: red, status: 302 };
  const out: Record<string, unknown> = { ...red };
  if (out.status === undefined) out.status = 302;
  return out;
}

/**
 * Spec §8: `fn.config.idempotencyTtl: { hours: 168 }` → wire field
 * `idempotency_ttl_hours: 168`. The authoring surface uses an object
 * so future TTL units (`days`, `minutes`) can land without breaking
 * the existing shape; the wire stores hours since that's what the
 * gateway honours.
 *
 * Returns `undefined` when the input is not a plain `{ hours }`
 * object; callers drop the field (gateway falls back to the 24h
 * default). Out-of-band values (≤0, ≥168) are clamped here so the
 * wire never carries a value the gateway would reject.
 *
 * Min 1, max 168 (7 days). Mirrors the Rust-side `clamp_ttl_hours`.
 */
function extractIdempotencyTtlHours(value: unknown): number | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  const rec = value as Record<string, unknown>;
  const h = rec.hours;
  if (typeof h !== "number" || !Number.isFinite(h)) return undefined;
  const rounded = Math.round(h);
  if (rounded < 1) return 1;
  if (rounded > 168) return 168;
  return rounded;
}

/**
 * Convert a single authored resource node to the wire shape. Drops
 * `children:` (handled separately) and renames camelCase keys.
 */
function authorToWire(node: Record<string, unknown>): WireResource {
  const out: WireResource = {};
  for (const [k, v] of Object.entries(node)) {
    if (k === "children") continue;
    if (k === "cache" && v && typeof v === "object") {
      out.cache = renameCacheControlKeys(v as Record<string, unknown>);
      continue;
    }
    if (k === "cors" && v && typeof v === "object") {
      out.cors = renameCorsKeys(v as Record<string, unknown>);
      continue;
    }
    if (k === "redirect" && v != null) {
      out.redirect = renameRedirectKeys(v as string | Record<string, unknown>);
      continue;
    }
    if (k === "idempotencyTtl") {
      const hours = extractIdempotencyTtlHours(v);
      if (hours !== undefined) {
        out.idempotency_ttl_hours = hours;
      }
      continue;
    }
    const renamed = CAMEL_TO_SNAKE[k] ?? k;
    out[renamed] = v;
  }
  return out;
}

// ── Tree flattening ────────────────────────────────────────────────────────

/**
 * Flatten an authored resource tree (potentially nested via `children:`)
 * into the wire's flat key map.
 *
 * Rules per §7 Authoring:
 *   - `rpc:` namespace uses `.` as the child separator.
 *   - URL namespace (key starts with `/`) uses `/`.
 *   - Bare `*` is the root sentinel; never has children.
 */
function flattenAuthorTree(
  tree: Record<string, Record<string, unknown>>,
): Record<string, WireResource> {
  const out: Record<string, WireResource> = {};
  for (const [key, node] of Object.entries(tree)) {
    flattenInto(out, key, node);
  }
  return out;
}

function flattenInto(
  out: Record<string, WireResource>,
  key: string,
  node: Record<string, unknown>,
): void {
  out[key] = authorToWire(node);
  const children = node.children as Record<string, Record<string, unknown>> | undefined;
  if (!children) return;
  const sep = childSeparator(key);
  for (const [childKey, childNode] of Object.entries(children)) {
    const fullKey = key + sep + childKey;
    flattenInto(out, fullKey, childNode);
  }
}

function childSeparator(parentKey: string): string {
  if (parentKey === "*") {
    throw new Error(`children: not allowed on the root "*" resource`);
  }
  if (parentKey.startsWith("rpc:")) return ".";
  if (parentKey.startsWith("/")) return "/";
  throw new Error(
    `unrecognized resource-key namespace: ${JSON.stringify(parentKey)}`,
  );
}

// ── Canonical JSON (used by override-marker shadow detection) ─────────────

function canonicalJson(value: unknown): string {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value !== null && typeof value === "object") {
    const sorted: Record<string, unknown> = {};
    for (const k of Object.keys(value as Record<string, unknown>).sort()) {
      sorted[k] = sortKeys((value as Record<string, unknown>)[k]);
    }
    return sorted;
  }
  return value;
}

// ── WireId resolution ──────────────────────────────────────────────────────

/**
 * Result of `pickWireId`: the wireId itself plus a tag describing
 * which resolution step produced it. The tag drives the
 * production-mode gate (only `default` is rejected) and is harmless
 * to throw away after assignment.
 */
type WireIdResolution = {
  wireId: string;
  source: "explicit" | "default";
};

/**
 * Pick the wireId for a procedure. Resolution order (highest priority
 * first; see `docs/proposals/rpc.md` §2):
 *
 *   1. `proc.config.id` (explicit, set on the procedure or on the
 *      module).
 *   2. Default: bare `<exportName>` — no path-derived slug.
 *
 * The returned wireId never has the `rpc:` prefix; the prefix is
 * applied when keying the resource map. The collision check and
 * production-mode gate run on the assigned wireIds — see
 * `computeManifestExtras()`.
 */
function pickWireId(proc: DiscoveredProcedure): WireIdResolution {
  // 1. Explicit id pinned on the procedure's `config.id`.
  const explicit = proc.config?.id;
  if (typeof explicit === "string" && explicit.length > 0) {
    return { wireId: explicit, source: "explicit" };
  }

  // 2. Default — bare export name. Path-derived slugs are NOT used
  //    here (they leak file structure to the wire). This is the only
  //    resolution that the production-mode gate rejects.
  return { wireId: proc.exportName, source: "default" };
}

// ── Validation ─────────────────────────────────────────────────────────────

const KEY_FORMAT_RE = /^(?:\*|rpc:[a-zA-Z0-9._*-]+|\/[\w\-/.\[\]:*]*)$/;

/**
 * Validate the authored, flattened resource map against the rules in
 * `docs/proposals/rpc.md` §7 ("Validation"). Throws (production) or
 * warns (dev) on failure.
 */
function validateResources(
  flat: Record<string, WireResource>,
  mode: "production" | "development",
  warn: (msg: string) => void,
): void {
  const errors: string[] = [];
  const warnings: string[] = [];

  // Resource-key format.
  for (const key of Object.keys(flat)) {
    if (!KEY_FORMAT_RE.test(key)) {
      errors.push(
        `resource key ${JSON.stringify(key)} does not match the allowed shape ` +
          `(bare "*", "rpc:<id>", or "/<path>")`,
      );
    }
  }

  // At-most-one routing action per resource.
  for (const [key, node] of Object.entries(flat)) {
    const actions = ["redirect", "rewrite", "static"].filter((a) => a in node);
    if (actions.length > 1) {
      errors.push(
        `resource ${JSON.stringify(key)} has multiple routing actions: ` +
          actions.join(", ") +
          ` — only one of redirect/rewrite/static is allowed`,
      );
    }
  }

  // Secure-by-default.
  for (const [key, node] of Object.entries(flat)) {
    if (node.auth === "anon" && node.publicly_accessible !== true) {
      const msg =
        `resource ${JSON.stringify(key)} sets auth: "anon" without publicly_accessible: true. ` +
        `Add publicly_accessible: true to confirm this is an intentionally public endpoint, ` +
        `or set auth: "user" or "admin".`;
      if (mode === "production") errors.push(msg);
      else warnings.push(msg);
    }
  }

  // Override marker — child weakening or shadowing inherited fields.
  // Fields we track for shadow-detection. `auth` is the prime case; other
  // fields are detected the same way but the build only emits a warning
  // for them — the spec doesn't explicitly forbid e.g. weakening rate_limit
  // (the merge rule is "min" anyway) but it does require declarative
  // intent for `auth`.
  const authStrength: Record<string, number> = { anon: 0, user: 1, admin: 2 };
  const shadowableFields = [
    "auth",
    "rate_limit",
    "max_input_bytes",
    "cache",
    "cors",
    "csrf_origins",
    "publicly_accessible",
    "idempotent",
    "idempotency_ttl_hours",
    "middleware",
  ];
  for (const [key, node] of Object.entries(flat)) {
    const parentKey = parentResourceKey(key);
    if (!parentKey || !flat[parentKey]) continue;
    const parent = flat[parentKey];
    const declaredOverrides = new Set(
      Array.isArray(node.override) ? (node.override as string[]) : [],
    );
    for (const field of shadowableFields) {
      if (!(field in node)) continue;
      if (!(field in parent)) continue;
      // Same value? Not a shadow.
      if (canonicalJson(node[field]) === canonicalJson(parent[field])) continue;
      // For auth, the spec only requires `override` when the child weakens.
      if (field === "auth") {
        const childN = authStrength[node.auth as string] ?? -1;
        const parentN = authStrength[parent.auth as string] ?? -1;
        // Strengthening (child >= parent) is fine without override.
        if (childN >= parentN) continue;
      }
      if (!declaredOverrides.has(field)) {
        errors.push(
          `resource ${JSON.stringify(key)} shadows inherited field ` +
            `${JSON.stringify(field)} from ${JSON.stringify(parentKey)} ` +
            `but does not list it in override: [...]. ` +
            `Add override: ${JSON.stringify([...declaredOverrides, field])} to confirm.`,
        );
      }
    }
  }

  for (const w of warnings) warn(w);
  if (errors.length > 0) {
    throw new Error(
      `[zeroship:manifest] validation failed:\n  - ` + errors.join("\n  - "),
    );
  }
}

/**
 * Walk up to the parent resource key. For URL paths we drop the last
 * segment (`/api/admin/users` → `/api/admin`). For RPC ids we drop the
 * last dot segment (`rpc:todos.delete` → `rpc:todos`). The bare `*`
 * is everyone's ultimate parent — but we don't infer it as a parent
 * for this check; `docs/proposals/rpc.md` §7 talks about explicit
 * parent declarations.
 */
function parentResourceKey(key: string): string | null {
  if (key === "*") return null;
  if (key.startsWith("rpc:")) {
    const id = key.slice("rpc:".length);
    const i = id.lastIndexOf(".");
    if (i < 0) return null;
    return "rpc:" + id.slice(0, i);
  }
  if (key.startsWith("/")) {
    const i = key.lastIndexOf("/");
    if (i <= 0) return null;
    return key.slice(0, i);
  }
  return null;
}

// ── defineApp config extraction ────────────────────────────────────────────
//
// Current limitation: we extract the `defineApp({ resources: { ... } })`
// argument via a coarse JS-evaluation approach. The user file is read,
// the import lines stripped, and the `defineApp(...)` call evaluated as
// a literal. Computed expressions (e.g. `auth: env.PROD ? ... : ...`)
// fail with a clear message asking the user to flatten the literal.
//
// This is the simplest possible extraction that handles the documented
// happy path. A full ts-morph based parse is future work.

async function loadDefineAppResources(
  root: string,
  configPath?: string,
): Promise<Record<string, Record<string, unknown>> | null> {
  // Exactly one canonical path: `src/server/config.ts`. The vite-plugin
  // never reads `zeroship.config.ts` at the project root, never reads
  // per-directory `$config.ts` — there is one place app-level defaults
  // and the resource tree live, and that is `src/server/config.ts`.
  // (Tests / advanced callers may override via `configPath`.)
  const path = configPath
    ? resolve(root, configPath)
    : resolve(root, "src/server/config.ts");

  let src: string;
  try {
    src = await fs.readFile(path, "utf8");
  } catch {
    return null;
  }
  return extractDefineAppLiteral(src, path);
}

/**
 * Extract the literal object passed to `defineApp({ ... })`.
 *
 * Approach: locate the `defineApp(` call, parse the matched-paren
 * region, then `Function`-eval the slice as a JS expression in a
 * scope where references to non-literal identifiers throw a useful
 * error. This intentionally supports only literal trees today.
 * Regex literals, template interpolation, and other computed
 * expressions are outside the contract and should be flattened
 * before they reach `defineApp({ resources })`.
 */
export function extractDefineAppLiteral(
  src: string,
  filePath: string,
): Record<string, Record<string, unknown>> | null {
  // Strip TS-only syntax that's harmless to evaluate-time JS:
  //   - TS type annotations on var/let/const (limited support)
  //   - import statements (we don't need them for the literal)
  //   - `as Foo` casts (rare in defineApp args, but possible)
  const stripped = src
    // Drop import lines.
    .replace(/^\s*import[\s\S]+?;[\r\n]+/gm, "")
    // Drop export keyword on default-export lines so we can pull the
    // expression on its own.
    .replace(/^\s*export\s+default\s+/m, "var __zsApp = ")
    // Strip `as Foo` type assertions.
    .replace(/\s+as\s+[A-Za-z_$][\w$<>,\s|&\[\]]*/g, "");

  // Find the defineApp(...) call.
  const m = stripped.match(/defineApp\s*\(/);
  if (!m || m.index === undefined) return null;
  const open = m.index + m[0].length;
  const close = matchParen(stripped, open - 1);
  if (close < 0) {
    throw new Error(
      `[zeroship:manifest] failed to parse defineApp(...) in ${filePath}: ` +
        `unbalanced parentheses`,
    );
  }
  const argSrc = stripped.slice(open, close).trim();
  // Empty arg list?
  if (argSrc === "") return null;

  // Best-effort eval: build a function that returns the expression,
  // catching ReferenceErrors with a spec-friendly message.
  let arg: unknown;
  try {
    // eslint-disable-next-line no-new-func
    arg = new Function(`return (${argSrc});`)();
  } catch (e) {
    throw new Error(
      `[zeroship:manifest] cannot evaluate defineApp argument in ${filePath} as a literal. ` +
        `This build only supports literal resource trees (no computed expressions). ` +
        `Hint: replace dynamic values like \`env.PROD ? "anon" : "user"\` with a constant. ` +
        `Underlying error: ${(e as Error).message}`,
    );
  }
  if (!arg || typeof arg !== "object") return null;
  const resources = (arg as { resources?: unknown }).resources;
  if (!resources || typeof resources !== "object") return null;
  return resources as Record<string, Record<string, unknown>>;
}

function matchParen(src: string, openIdx: number): number {
  if (src[openIdx] !== "(") return -1;
  let depth = 0;
  let i = openIdx;
  let inString: string | null = null;
  let inLineComment = false;
  let inBlockComment = false;
  while (i < src.length) {
    const c = src[i];
    const next = src[i + 1];
    if (inLineComment) {
      if (c === "\n") inLineComment = false;
      i++;
      continue;
    }
    if (inBlockComment) {
      if (c === "*" && next === "/") {
        inBlockComment = false;
        i += 2;
        continue;
      }
      i++;
      continue;
    }
    if (inString) {
      if (c === "\\") {
        i += 2;
        continue;
      }
      if (c === inString) inString = null;
      i++;
      continue;
    }
    if (c === "/" && next === "/") {
      inLineComment = true;
      i += 2;
      continue;
    }
    if (c === "/" && next === "*") {
      inBlockComment = true;
      i += 2;
      continue;
    }
    if (c === '"' || c === "'" || c === "`") {
      inString = c;
      i++;
      continue;
    }
    if (c === "(") depth++;
    else if (c === ")") {
      depth--;
      if (depth === 0) return i;
    }
    i++;
  }
  return -1;
}

// ── Auto-derived RPC entries ───────────────────────────────────────────────

/**
 * Build the wire shape for an auto-derived RPC procedure entry. Pulls
 * kind/idempotent/auth/rate_limit/etc. from `proc.config` and
 * `proc.moduleConfig`.
 *
 * Schemas (Zod) are NOT serialized into the manifest — they're typed
 * objects the synthetic SSR entry calls `.parse()` on at runtime. The
 * `input` / `output` keys on `proc.config` (when present) are skipped
 * here so the wire stays clean.
 */
function autoDeriveRpcEntry(proc: DiscoveredProcedure): WireResource {
  const out: WireResource = {};
  // B3: `action` is the SDK-side capability tag. The Rust-side
  // `ProcedureKind` enum currently has {Query, Mutation, Stream,
  // Subscription} only — adding `Action` to the wire format would
  // ripple through bundle parsing, gateway dispatch, integration
  // tests. For now we ship the capability typing on the SDK side and
  // omit `kind` from the manifest for action procedures. The gateway
  // treats "no kind" as "no method/transport restriction", which is
  // exactly action's semantics (most permissive).
  if (proc.kind !== "action") {
    out.kind = proc.kind;
  }

  // Module-level $config has lowest priority among user-supplied metadata.
  // Per-procedure config wins.
  const merged: Record<string, unknown> = {
    ...(proc.moduleConfig ?? {}),
    ...(proc.config ?? {}),
  };
  delete merged.id; // wireId is keyed separately, never written into the resource.
  // Zod schemas live in fn.config.input / fn.config.output but never
  // touch the wire — runtime validation owns them.
  delete merged.input;
  delete merged.output;

  // Apply via authorToWire so camelCase → snake_case conversion is
  // identical to the user-tree path.
  const wired = authorToWire(merged);
  Object.assign(out, wired);

  return out;
}

// ── Public entry point ─────────────────────────────────────────────────────

export async function computeManifestExtras(
  input: ManifestExtrasInput,
): Promise<ManifestExtras> {
  const { root, procedures, mode, configPath } = input;
  const onWarn = input.onWarn ?? ((msg) => console.warn(`[zeroship:manifest] ${msg}`));

  // 1. Resolve every procedure's wireId, building both the auto-derived
  //    resources block and a parallel array of (resolution, proc) pairs
  //    for the collision-detection and production-mode-gate passes
  //    below.
  type Assigned = {
    proc: DiscoveredProcedure;
    resolution: WireIdResolution;
    resourceKey: string;
  };
  const assignments: Assigned[] = [];
  const autoResources: Record<string, WireResource> = {};

  for (const proc of procedures) {
    const resolution = pickWireId(proc);
    const resourceKey = `rpc:${resolution.wireId}`;
    assignments.push({ proc, resolution, resourceKey });
  }

  // 2. Collision check — two distinct procedures that resolved to the
  //    same wireId. This is unrecoverable: the wire path
  //    `/__zeroship/v1/<wireId>` would be ambiguous. Cite both file paths and
  //    instruct the user to pin an explicit id.
  const byKey = new Map<string, Assigned[]>();
  for (const a of assignments) {
    const arr = byKey.get(a.resourceKey);
    if (arr) arr.push(a);
    else byKey.set(a.resourceKey, [a]);
  }
  const collisions: string[] = [];
  for (const [key, group] of byKey) {
    if (group.length < 2) continue;
    // Same (filePath, exportName) being recorded twice (e.g. the
    // transform fires on multiple environments) is not a real
    // collision — dedup before flagging.
    const distinct = new Map<string, Assigned>();
    for (const a of group) {
      distinct.set(`${a.proc.filePath}::${a.proc.exportName}`, a);
    }
    if (distinct.size < 2) continue;
    const lines = [...distinct.values()].map(
      (a) => `    ${a.proc.filePath} (export ${JSON.stringify(a.proc.exportName)})`,
    );
    collisions.push(
      `wireId collision: ${JSON.stringify(key)} is the default for multiple procedures:\n${lines.join("\n")}\n` +
        `    Pin an explicit id on at least one of them, e.g.:\n` +
        `        export function ${[...distinct.values()][0].proc.exportName}(...) { ... }\n` +
        `        ${[...distinct.values()][0].proc.exportName}.config = { id: "<unique>" };`,
    );
  }
  if (collisions.length > 0) {
    throw new Error(
      `[zeroship:manifest] ` + collisions.join("\n"),
    );
  }

  // 3. Production-mode gate — any procedure whose wireId came from the
  //    bare-name default (resolution.source === "default") is rejected
  //    in production. The wire identity for a deployed app must be
  //    explicit, not implicit.
  if (mode === "production") {
    const offenders = assignments.filter((a) => a.resolution.source === "default");
    if (offenders.length > 0) {
      const lines = offenders.map((a) => {
        const name = a.proc.exportName;
        return (
          `  - procedure ${JSON.stringify(name)} (in ${a.proc.filePath}) has no explicit \`id\`. ` +
          `Add \`${name}.config = { id: "..." }\` before deploying.`
        );
      });
      throw new Error(
        `[zeroship:manifest] production build refused: ` +
          `every procedure must have an explicit \`id\`.\n` +
          lines.join("\n"),
      );
    }
  }

  for (const a of assignments) {
    autoResources[a.resourceKey] = autoDeriveRpcEntry(a.proc);
  }

  // 4. defineApp({ resources }) tree from src/server/config.ts.
  const userTree = await loadDefineAppResources(root, configPath);
  const userFlat = userTree ? flattenAuthorTree(userTree) : {};

  // 5. Merge: auto-derived first, user entries on top (user wins).
  const merged: Record<string, WireResource> = { ...autoResources };
  for (const [key, node] of Object.entries(userFlat)) {
    if (key in merged) {
      merged[key] = { ...merged[key], ...node };
    } else {
      merged[key] = node;
    }
  }

  // 6. Validate.
  validateResources(merged, mode, onWarn);

  return {
    resources: merged,
    transformer: "json",
    versionHint: 1,
  };
}
