import type { Plugin } from "vite";
import { relative, extname } from "node:path";
import MagicString from "magic-string";

/**
 * Source modules whose named exports the transform recognizes as RPC
 * procedure wrappers (`procedure`/`query`/`mutation`/`action`/`stream`/
 * `subscription`).
 *
 * Wrappers live at `@zeroship/rpc/server`. `@zeroship/server` owns
 * `defineApp`, `z`, and runtime substrate helpers.
 */
const WRAPPER_SOURCES = new Set(["@zeroship/rpc/server"]);

/** Names exported by {@link WRAPPER_SOURCES} that mark an export as RPC. */
const WRAPPER_NAMES = new Set([
  "procedure",
  "query",
  "mutation",
  "action",
  "stream",
  "subscription",
]);

/** Wrapper-marker discriminator. `procedure` is generic; the others
 *  imply a kind the transform reads statically.
 *
 *  B3 capability mapping (see `docs/proposals/zeroship-db.md` §B3):
 *    - `query`  → DB-read tx, no fetch
 *    - `mutation` → DB-write tx, no fetch
 *    - `action` / `stream` / `subscription` / `procedure` →
 *      action-capability (no surrounding tx, fetch allowed). The
 *      manifest emitter folds non-`action` legacy kinds back to their
 *      original `kind` field for backwards-compat. */
type WrapperKind =
  | "query"
  | "mutation"
  | "action"
  | "stream"
  | "subscription"
  | "procedure";

/**
 * Per-procedure metadata stashed at transform time. Consumed by the
 * manifest emitter (`src/manifest.ts`) at closeBundle to build the
 * `manifest.resources` block.
 *
 * Wire-stable contract: this struct mirrors `DiscoveredProcedure` in
 * `src/manifest.ts`. They evolve together.
 *
 * `config.input` / `config.output`: Zod schemas (or any object with a
 * `.parse()` method). They are typed objects the synthetic SSR entry
 * calls at runtime via `fn.config.input.parse(args)`. The transform
 * never reads or executes them — they exist as identifier references
 * in the user module that survive bundling.
 */
export interface DiscoveredProcedureRecord {
  filePath: string;
  exportName: string;
  moduleSlug: string;
  /**
   * Procedure kind, as resolved by the transform. `action` lands here
   * when the user wrote `action(...)` explicitly. `procedure(...)`
   * resolves through `defaultKind()` so async generators still become
   * streams while generic unary procedures default to mutations.
   *
   * The Rust-side wire `ProcedureKind` enum currently lacks `action`
   * (it predates B3). The manifest emitter folds `action` → `mutation`
   * for the current Rust manifest enum; runtime capability gates
   * enforce the actual behaviour.
   */
  kind: "query" | "mutation" | "action" | "stream" | "subscription";
  isStream: boolean;
  config?: Record<string, unknown>;
  moduleConfig?: Record<string, unknown>;
  /** Wave #188 — opt-in lazy procedure flag, set when the procedure's
   *  config carries `lazy: true` as a literal boolean (either via
   *  `<fn>.config.lazy = true` or the `query(handler, { lazy: true })`
   *  wrapper option). Drives the synthetic-entry generator's
   *  dynamic-import wrapper emission. Non-literal `lazy` expressions
   *  warn at build time and fall back to `false`. */
  lazy?: boolean;
}

export interface TransformState {
  /**
   * Map from project-relative file path to the set of exported server
   * function names. Populated as each server module is transformed;
   * consumed by the build report and for export signature tracking.
   */
  serverFunctionMap: Map<string, Set<string>>;
  /**
   * Accumulated per-procedure metadata for the manifest emitter.
   * Keyed by `${filePath}::${exportName}` to avoid duplicates when the
   * transform runs in multiple environments (`ssr` + dev `zeroship`).
   */
  discoveredProcedures: DiscoveredProcedureRecord[];
}

// --- AST helpers (work with Rolldown's Oxc/ESTree AST) ---

/**
 * File-level `"use server"` directive detector.
 *
 * The old path convention (`src/server.{ts,tsx,js,jsx}` single-file
 * layout, anything under `src/server/**` directory layout) is gone.
 * It silently turned every exported function — including helpers
 * reached via `export * from "./helpers"` — into a public, network-
 * reachable RPC endpoint.
 *
 * The replacement: a file is a server module iff it opens with the
 * ECMAScript Directive Prologue string-literal expression statement
 * `"use server"`. Per the spec, only string-literal expression
 * statements at the top of the body count as directives, BEFORE the
 * first non-string-expression statement. Comments are stripped by the
 * parser; we just look at body[0].
 *
 * Note this is the file-level marker only. A later function-level
 * `"use server"` mode may let one file mix client and server code; for
 * now a file is wholly server or wholly client.
 */
export function detectFileLevelUseServer(ast: { body?: unknown[] }): boolean {
  const body = ast.body;
  if (!Array.isArray(body) || body.length === 0) return false;
  const first = body[0] as
    | {
        type?: string;
        directive?: string;
        expression?: { type?: string; value?: unknown; raw?: string };
      }
    | undefined;
  if (!first || first.type !== "ExpressionStatement") return false;
  // Acorn (used in tests + dev-mode parse) sets `directive` on the
  // ExpressionStatement when the expression is a directive prologue
  // string literal. Rolldown/Oxc may not set it; fall back to inspecting
  // the expression's literal value. Both shapes are normalized here.
  if (typeof first.directive === "string") {
    return first.directive === "use server";
  }
  const expr = first.expression;
  if (!expr) return false;
  if (expr.type !== "Literal" && expr.type !== "StringLiteral") return false;
  return expr.value === "use server";
}

/**
 * Function-level `"use server"` directive detector.
 *
 * `docs/proposals/rpc.md` §1 says that a function whose first
 * statement is the string
 * literal `"use server"` is a server function regardless of whether
 * the enclosing file carries a file-level directive. The function may
 * be declared via:
 *
 *   - `function name() { "use server"; ... }` (FunctionDeclaration)
 *   - `const name = async () => { "use server"; ... }` (Arrow + VarDecl)
 *   - `const name = function() { "use server"; ... }` (FnExpr + VarDecl)
 *   - `name = async function() { "use server"; ... }` (assignment)
 *   - `export function name() { "use server"; ... }`
 *
 * The walk covers nested + top-level declarations. Returns the set of
 * names that resolve to a marked function.
 *
 * A function-level directive makes ONLY that function a server
 * reference; other code in the file stays client-side. The returned
 * names feed both the per-file metadata pass and the reference-graph
 * walk in `server-graph.ts`.
 */
export function detectFunctionLevelUseServer(ast: { body?: unknown[] }): Set<string> {
  const out = new Set<string>();
  const body = ast.body;
  if (!Array.isArray(body)) return out;

  // Recursive AST walker. We only care about (a) function-like nodes
  // that carry a `"use server"` directive, and (b) the binding-name
  // lookup that ties the function back to a public identifier.
  function walk(node: unknown, parentBindingName: string | undefined): void {
    if (!node || typeof node !== "object") return;
    const n = node as Record<string, unknown> & { type?: string };
    const t = n.type;

    // Function-like nodes — check for the directive at body[0].
    if (
      t === "FunctionDeclaration" ||
      t === "FunctionExpression" ||
      t === "ArrowFunctionExpression"
    ) {
      const fnBody = n.body as { type?: string; body?: unknown[] } | undefined;
      // Arrow with expression body (`() => x`) cannot carry a directive.
      if (fnBody && fnBody.type === "BlockStatement" && Array.isArray(fnBody.body)) {
        const first = fnBody.body[0] as
          | {
              type?: string;
              directive?: string;
              expression?: { type?: string; value?: unknown };
            }
          | undefined;
        const isDirective =
          first?.type === "ExpressionStatement" &&
          (first.directive === "use server" ||
            (first.expression &&
              (first.expression.type === "Literal" ||
                first.expression.type === "StringLiteral") &&
              first.expression.value === "use server"));
        if (isDirective) {
          // Resolve the function's binding name. FunctionDeclarations
          // carry `id.name` directly; FunctionExpressions / Arrows are
          // anonymous unless captured by a parent VariableDeclarator
          // / AssignmentExpression / Property (object literal value).
          if (t === "FunctionDeclaration") {
            const id = n.id as { name?: string } | undefined;
            if (id?.name) out.add(id.name);
          } else if (parentBindingName) {
            out.add(parentBindingName);
          }
        }
      }
      // Recurse into the body so nested marked functions are found too.
      if (fnBody && Array.isArray((fnBody as { body?: unknown[] }).body)) {
        for (const child of (fnBody as { body?: unknown[] }).body!) walk(child, undefined);
      }
      return;
    }

    // Bind-resolving wrappers: VariableDeclarator and AssignmentExpression
    // pass their LHS name down so an anonymous function-expression /
    // arrow on the RHS can claim it.
    if (t === "VariableDeclaration") {
      for (const d of (n.declarations ?? []) as Array<{
        id?: { type?: string; name?: string };
        init?: unknown;
      }>) {
        const name = d.id?.type === "Identifier" ? d.id.name : undefined;
        walk(d.init, name);
      }
      return;
    }
    if (t === "AssignmentExpression") {
      const left = n.left as { type?: string; name?: string } | undefined;
      const name = left?.type === "Identifier" ? left.name : undefined;
      walk(n.right, name);
      return;
    }

    // Generic recursion — visit every child key. Ignore parent /
    // location metadata.
    for (const key of Object.keys(n)) {
      if (key === "type" || key === "loc" || key === "range" || key === "start" || key === "end") {
        continue;
      }
      const v = (n as Record<string, unknown>)[key];
      if (Array.isArray(v)) {
        for (const item of v) walk(item, undefined);
      } else if (v && typeof v === "object") {
        walk(v, undefined);
      }
    }
  }

  for (const stmt of body) walk(stmt, undefined);
  return out;
}

/**
 * Path predicate for the legacy `src/server.{ts,tsx,js,jsx}` /
 * `src/server/**` shape. The path convention itself is gone, but the
 * predicate stays around so the transform can emit a friendly
 * "you probably want a `"use server"` directive at the top of this
 * file" hint when a developer trips over the breaking change.
 */
export function looksLikeLegacyServerPath(root: string, filePath: string): boolean {
  const rel = relative(root, filePath).replace(/\\/g, "/");
  if (rel.startsWith("..")) return false;
  if (/^src\/server\.(ts|tsx|js|jsx)$/.test(rel)) return true;
  if (/^src\/server\//.test(rel) && /\.(ts|tsx|js|jsx)$/.test(rel)) return true;
  return false;
}

/**
 * Cheap textual pre-filter for the `"use server"` directive. Skips
 * the optional BOM/shebang, leading whitespace, and line/block
 * comments, then checks whether the very next token is a string
 * literal whose value is `use server`. This avoids the AST parse cost
 * on the >99% of source files that don't open with the directive —
 * `detectFileLevelUseServer()` is the source of truth.
 *
 * False positives (returns true when the AST detector would say no)
 * are harmless: the parse runs and the AST detector decides
 * authoritatively. False negatives would silently drop server
 * modules; the walker below is conservative enough to handle every
 * shape the parser tolerates at the directive position.
 */
function quickHasUseServerDirective(code: string): boolean {
  let i = 0;
  if (code.charCodeAt(0) === 0xfeff) i = 1;
  if (code.startsWith("#!", i)) {
    const nl = code.indexOf("\n", i);
    i = nl < 0 ? code.length : nl + 1;
  }
  while (i < code.length) {
    const c = code[i];
    if (c === " " || c === "\t" || c === "\n" || c === "\r") {
      i++;
      continue;
    }
    if (c === "/" && code[i + 1] === "/") {
      const nl = code.indexOf("\n", i + 2);
      i = nl < 0 ? code.length : nl + 1;
      continue;
    }
    if (c === "/" && code[i + 1] === "*") {
      const end = code.indexOf("*/", i + 2);
      i = end < 0 ? code.length : end + 2;
      continue;
    }
    break;
  }
  if (i >= code.length) return false;
  const head = code.slice(i, i + 12);
  return head === '"use server"' || head === "'use server'";
}

/**
 * Per-file symbol table mapping locally-bound identifiers to the
 * wrapper marker name they resolve to.
 *
 *   import { procedure, query as q } from "@zeroship/rpc/server";
 *   // bindings: { procedure → "procedure", q → "query" }
 *
 * Only named imports from {@link WRAPPER_SOURCES} are recorded;
 * default imports, namespace imports (`import * as ns`), and
 * re-exports of wrapper names are NOT considered markers — the import
 * must be a direct named import of a known wrapper from a known
 * package, so the symbol table is decidable from the AST alone.
 */
function collectWrapperBindings(astBody: any[]): Map<string, WrapperKind> {
  const bindings = new Map<string, WrapperKind>();
  for (const node of astBody) {
    if (node.type !== "ImportDeclaration") continue;
    const sourceLit = node.source;
    if (!sourceLit || typeof sourceLit.value !== "string") continue;
    if (!WRAPPER_SOURCES.has(sourceLit.value)) continue;
    for (const spec of node.specifiers || []) {
      if (spec.type !== "ImportSpecifier") continue;
      const imported = spec.imported?.name;
      const local = spec.local?.name;
      if (!imported || !local) continue;
      if (!WRAPPER_NAMES.has(imported)) continue;
      bindings.set(local, imported as WrapperKind);
    }
  }
  return bindings;
}

/**
 * Inspect a `VariableDeclarator` initializer to see if it's a wrapper
 * call. Returns `{ kind, handler, configNode }` when the callee is an
 * Identifier bound to a wrapper marker (per {@link
 * collectWrapperBindings}); else `null`.
 *
 *   procedure(async (x) => x)              → { kind: "procedure", handler: ArrowFn,    configNode: undefined }
 *   query(handler, { id: "list" })         → { kind: "query",     handler: Identifier, configNode: ObjectExpression }
 *   list(...)                              → null   (callee is not bound to a wrapper)
 *
 * Member-expression callees (`mod.procedure(...)`, `pkg.query(...)`)
 * are NOT recognized — wrappers must be bare identifier calls so they
 * survive minification under a stable name AND so the static symbol
 * table is enough to decide.
 *
 * The `configNode` is the second arg as an AST node (the literalize()
 * pass converts it to a plain object). Returned UNINTERPRETED here so
 * the caller can apply `allowSchemaProps: true` (preserves Zod
 * `input` / `output` keys as marker sentinels).
 */
function matchWrapperCall(
  init: any,
  bindings: Map<string, WrapperKind>,
): { kind: WrapperKind; handler: any; configNode: any } | null {
  if (!init || init.type !== "CallExpression") return null;
  const callee = init.callee;
  if (!callee || callee.type !== "Identifier") return null;
  const wrapper = bindings.get(callee.name);
  if (!wrapper) return null;
  const handler = init.arguments?.[0];
  const configNode = init.arguments?.[1];
  return { kind: wrapper, handler, configNode };
}

/** Import prelude emitted once per transformed client module. Pulls
 *  the public generated-stub factory from `@zeroship/rpc/client`.
 *  The factory owns transport configuration and procedure branding. */
const CLIENT_IMPORT_PRELUDE = `import { createRpcClient } from "@zeroship/rpc/client";\nconst __zsRpc = createRpcClient();\n`;

/** Shared wire helpers: intentionally empty. Client stubs delegate to
 *  `@zeroship/rpc/client`, so generated and manual clients share the
 *  same transport, transformer, retry, and error behavior. */
const CLIENT_HELPERS = "";

/**
 * Slug a module file path into a stable id segment.
 *
 *   `src/server/todos.ts` → `src-server-todos`
 *
 * NOTE: the slug is NOT used in wireId derivation — the default wireId
 * is just the bare `<exportName>`.
 * The slug is retained only for diagnostic messages (the build report
 * and collision-error hints reference it for human readability).
 */
function moduleSlug(root: string, id: string): string {
  const rel = relative(root, id).replace(/\\/g, "/");
  const ext = extname(rel);
  const base = ext ? rel.slice(0, -ext.length) : rel;
  return base.replace(/[^a-zA-Z0-9._-]/g, "-");
}

/**
 * Pick the procedure kind when the wrapper did not pin one.
 *
 * Name-based query inference is intentionally not used: reads opt in via
 * the `query(...)` wrapper or an explicit `config.kind = "query"`. Generic
 * unary `procedure(...)` defaults to `mutation`, which is safer for caching,
 * retry, and capability policy.
 */
function defaultKind(
  isStream: boolean,
): "mutation" | "stream" {
  if (isStream) return "stream";
  return "mutation";
}

/**
 * Marker stored in `proc.config.input` / `proc.config.output` when the
 * user declared a Zod schema. The transform never runs the schema
 * (it's a typed runtime object); the synthetic SSR entry calls
 * `fn.config.input.parse(args)` at request time. This sentinel exists
 * so downstream code can detect declaration without inspecting the AST.
 */
export const ZS_SCHEMA_MARKER = Symbol.for("zeroship/zod-schema");

/** Branded marker shape for serialization-friendly comparisons. */
export interface ZsSchemaMarker {
  readonly __zsSchema: true;
}

const SCHEMA_MARKER: ZsSchemaMarker = Object.freeze({ __zsSchema: true });

/**
 * Convert an ESTree literal AST into a plain JS value. Supports the
 * literal subset documented for `<fnName>.config` and module-level
 * `$config`: primitives, arrays, plain objects.
 *
 * Anything else (identifier references, ternary expressions, function
 * calls) returns `undefined`. The manifest validator surfaces a clean
 * error if a procedure has computed metadata; we don't try to
 * recover those values here.
 *
 * Special-case: at the top level of a `<fnName>.config = { ... }`
 * literal, the keys `input` and `output` may carry **arbitrary
 * expressions** — typically Zod schemas (`z.object({...})`). We don't
 * literalize them; instead we record the {@link SCHEMA_MARKER}
 * sentinel so the rest of the literal still parses cleanly and the
 * manifest emitter can drop these keys before serializing.
 */
function literalize(node: any, opts?: { allowSchemaProps?: boolean }): unknown {
  if (!node) return undefined;
  switch (node.type) {
    case "Literal":
      return node.value;
    case "TemplateLiteral":
      // Only inline-untagged templates with no expressions are literal.
      if (node.expressions.length === 0 && node.quasis.length === 1) {
        return node.quasis[0].value.cooked ?? node.quasis[0].value.raw;
      }
      return undefined;
    case "ArrayExpression": {
      const out: unknown[] = [];
      for (const el of node.elements ?? []) {
        if (el == null) {
          out.push(undefined);
          continue;
        }
        const v = literalize(el);
        if (v === undefined) return undefined;
        out.push(v);
      }
      return out;
    }
    case "ObjectExpression": {
      const out: Record<string, unknown> = {};
      for (const p of node.properties ?? []) {
        if (p.type !== "Property" || p.computed || p.shorthand === undefined) {
          // Spread or computed key: bail.
          if (p.type !== "Property") return undefined;
        }
        const k = p.key?.type === "Identifier" ? p.key.name :
                  p.key?.type === "Literal" ? String(p.key.value) : undefined;
        if (k === undefined) return undefined;
        // Schema-declared properties (`input` / `output`) at the top
        // level of `fn.config = { ... }`. The value is an arbitrary
        // call expression (Zod schema) — we record presence with a
        // marker, never the AST itself.
        if (opts?.allowSchemaProps && (k === "input" || k === "output")) {
          out[k] = SCHEMA_MARKER;
          continue;
        }
        const v = literalize(p.value);
        if (v === undefined) return undefined;
        out[k] = v;
      }
      return out;
    }
    case "UnaryExpression":
      // Allow `-1`, `+1`, `!true`.
      if (node.operator === "-" || node.operator === "+") {
        const arg = literalize(node.argument);
        if (typeof arg === "number") {
          return node.operator === "-" ? -arg : +arg;
        }
      }
      return undefined;
    default:
      return undefined;
  }
}

/**
 * Inspect an ObjectExpression-typed AST node for a `lazy` key and
 * classify its value. Used by Wave #188 to decide whether a procedure
 * opts into the dynamic-import wrapper emission.
 *
 *   { lazy: true }   → "true"
 *   { lazy: false }  → "false"
 *   { lazy: someVar} → "non-literal"   (warns; treated as false)
 *   { ... }          → "absent"
 *
 * Kept separate from `literalize()` so a non-literal `lazy` expression
 * doesn't void the rest of the config — the user keeps their `id`,
 * `kind`, schemas, etc. and just loses the lazy opt-in.
 */
function inspectLazy(node: any): "true" | "false" | "non-literal" | "absent" {
  if (!node || node.type !== "ObjectExpression") return "absent";
  for (const p of node.properties ?? []) {
    if (p.type !== "Property" || p.computed) continue;
    const k =
      p.key?.type === "Identifier"
        ? p.key.name
        : p.key?.type === "Literal"
          ? String(p.key.value)
          : undefined;
    if (k !== "lazy") continue;
    const v = p.value;
    if (v?.type === "Literal" && typeof v.value === "boolean") {
      return v.value ? "true" : "false";
    }
    return "non-literal";
  }
  return "absent";
}

/**
 * Walk module body, collecting `<fnName>.config = { ... }` and
 * `export const $config = { ... }` assignments. Returns:
 *
 *   - perFn:    Map<exportName, configLiteral>
 *   - moduleConfig: literal of `$config` or undefined
 */
function collectConfig(astBody: any[]): {
  perFn: Map<string, Record<string, unknown>>;
  moduleConfig: Record<string, unknown> | undefined;
  /** Per-fn AST node for the legacy `<fn>.config = { ... }` RHS. Kept
   *  alongside the literalized `perFn` so Wave #188's lazy detector can
   *  classify non-literal `lazy` expressions without re-walking the body. */
  perFnNode: Map<string, any>;
} {
  const perFn = new Map<string, Record<string, unknown>>();
  const perFnNode = new Map<string, any>();
  let moduleConfig: Record<string, unknown> | undefined;

  for (const node of astBody) {
    // export const $config = { ... };
    if (
      node.type === "ExportNamedDeclaration" &&
      node.declaration?.type === "VariableDeclaration"
    ) {
      for (const d of node.declaration.declarations ?? []) {
        if (d.id?.type === "Identifier" && d.id.name === "$config" && d.init) {
          const lit = literalize(d.init);
          if (lit && typeof lit === "object" && !Array.isArray(lit)) {
            moduleConfig = lit as Record<string, unknown>;
          }
        }
      }
    }
    // <fnName>.config = { ... }; — at module scope.
    //
    // Top-level `input` / `output` keys may carry Zod schemas (arbitrary
    // call expressions). Pass `allowSchemaProps: true` so literalize()
    // tolerates them as opaque markers; the synthetic SSR entry reads
    // the runtime values via `fn.config.input` / `fn.config.output`.
    if (node.type === "ExpressionStatement" &&
        node.expression?.type === "AssignmentExpression" &&
        node.expression.operator === "=" &&
        node.expression.left?.type === "MemberExpression" &&
        node.expression.left.object?.type === "Identifier" &&
        node.expression.left.property?.type === "Identifier" &&
        node.expression.left.property.name === "config") {
      const fnName = node.expression.left.object.name;
      const rhs = node.expression.right;
      const lit = literalize(rhs, { allowSchemaProps: true });
      if (lit && typeof lit === "object" && !Array.isArray(lit)) {
        perFn.set(fnName, lit as Record<string, unknown>);
      }
      // Even when literalize() bails (non-literal value somewhere) we
      // record the AST node so the lazy inspector can still classify
      // its `lazy` key independently.
      perFnNode.set(fnName, rhs);
    }
  }
  return { perFn, moduleConfig, perFnNode };
}

/** Client stub for a non-streaming export.
 *
 * Single-input wire: the user's procedure takes one
 * value, so the stub forwards `args[0]` (or `undefined` when called
 * with no args). Procedures that conceptually take multiple values
 * pass them as a single object.
 *
 * Emits a call through the generated-stub factory from
 * `@zeroship/rpc/client`; the factory attaches the `__SERVER_REFERENCE`
 * brand plus `{ id, kind, wire }` metadata uniformly. Runtime callers can
 * detect stubs passed as props by checking the brand. */
function clientUnaryStub(
  name: string,
  methodName: string,
  kind: string,
  idempotent: boolean,
): string {
  const options = idempotent ? `, ${JSON.stringify({ idempotent: true })}` : "";
  const factory = kind === "action" ? "action" : kind === "query" ? "query" : "mutation";
  return `export const ${name} = __zsRpc.${factory}(${JSON.stringify(methodName)}${options});`;
}

/** Client stub for a streaming export. Subscriptions deliberately keep
 *  their metadata but route through the generic procedure factory so a
 *  client call fails with UNIMPLEMENTED instead of silently using the
 *  SSE stream transport. */
function clientStreamStub(name: string, methodName: string, kind: string): string {
  if (kind === "subscription") {
    return `export const ${name} = __zsRpc.procedure(${JSON.stringify({
      id: methodName,
      kind: "subscription",
      wire: "json",
    })});`;
  }
  return `export const ${name} = __zsRpc.stream(${JSON.stringify(methodName)});`;
}

export function transformPlugin(_rpcEndpoint: string, state: TransformState): Plugin {
  const { serverFunctionMap } = state;
  let root = "";
  // Track legacy-server-path files we've already warned about so HMR
  // / repeat transforms don't spam the console.
  const warnedLegacyPaths = new Set<string>();

  return {
    name: "zeroship:transform",
    enforce: "pre" as const,

    configResolved(resolvedConfig: any) {
      root = resolvedConfig.root;
    },

    transform: {
      filter: {
        id: { include: /\.(ts|tsx|js|jsx)$/, exclude: /node_modules/ },
      },
      handler(this: any, code: string, id: string) {
        // Server environment detection:
        //  - dev: `this.environment.name === "zeroship"` (dev-server creates it)
        //  - prod build: Vite's `ssr: entry` build runs in environment `ssr`
        //    (rolldown sets `this.environment.name === "ssr"`). We treat both
        //    as the server side.
        //  - prod client build: environment name is `"client"`.
        const envName = this.environment?.name;
        const isServerEnv = envName === "zeroship" || envName === "ssr";

        // 1. Cheap textual pre-filter — skip files that obviously can't
        //    be a server module without parsing. The directive must be
        //    the first non-trivial token after the BOM / whitespace /
        //    comments. This avoids the AST parse cost on the >99% of
        //    source files that don't open with `"use server"`.
        if (!quickHasUseServerDirective(code)) {
          // Friendly hint for code still laid out like the legacy
          // `src/server.{ts,...}` / `src/server/**` shape: a file that's
          // missing the directive is almost certainly an unmigrated
          // server module. Emit one warning per file path per dev
          // session so HMR doesn't spam.
          if (
            looksLikeLegacyServerPath(root, id) &&
            !warnedLegacyPaths.has(id)
          ) {
            warnedLegacyPaths.add(id);
            const rel = relative(root, id).replace(/\\/g, "/");
            const msg =
              `[zeroship:transform] ${rel} sits at the legacy server-module path ` +
              `but is missing the \`"use server"\` directive. ` +
              `Path-based discovery was dropped: add ` +
              `\`"use server";\` as the first line, then wrap each RPC ` +
              `export with procedure()/query()/mutation()/stream() ` +
              `from \`@zeroship/rpc/server\`. Untouched files will not be ` +
              `published as RPC endpoints.`;
            // pluginContext.warn is the structured Vite hook (carries
            // the file id, surfaces in the dev overlay). Fall back to
            // console.warn when running outside Vite (the test harness).
            if (typeof this.warn === "function") this.warn(msg);
            else console.warn(msg);
          }
          return null;
        }

        // 2. Parse AST with Rolldown's built-in parser.
        const isTsx = id.endsWith(".tsx") || id.endsWith(".jsx");
        const ast = this.parse(code, { lang: isTsx ? "tsx" : "ts" });

        // 3. Server-module gate — file-level `"use server"` directive.
        //    The path convention is gone; only this directive opts a
        //    file into RPC discovery.
        if (!detectFileLevelUseServer(ast)) return null;

        // 4. Find server-procedure exports inside the server module.
        //
        //    The file already declared `"use server"` at the top — so
        //    every code path below it is server-side — but unlike the
        //    legacy path-convention behavior, NOT every export is
        //    automatically an RPC. Only exports whose initializer is a
        //    call to one of the wrapper markers (`procedure`, `query`,
        //    `mutation`, `stream`, `subscription` imported from
        //    `@zeroship/rpc/server`) are registered.
        //
        //    Plain `export function helper(...)` and `export const x =
        //    ...` stay private; they survive in the server bundle and
        //    are callable by other server code, but they are NOT
        //    network-reachable. That means a
        //    misplaced `export * from "./helpers"` no longer publishes
        //    helpers as `/_zs/v1/<helperName>` endpoints.
        const wrapperBindings = collectWrapperBindings(ast.body);
        interface ServerFn {
          name: string;
          /** Wrapper kind from the marker call: query/mutation/stream/
           *  subscription (an explicit kind), or "procedure" (generic
           *  marker — kind comes from `defaultKind()`). */
          markerKind: WrapperKind;
          /** Config object pulled from the wrapper's second argument,
           *  e.g. `procedure(handler, { id: "x" })`. Already literalized
           *  with `allowSchemaProps: true` so Zod `input` / `output`
           *  call expressions survive as schema markers. */
          wrapperConfig: Record<string, unknown> | undefined;
          /** Wrapper's second-arg AST (raw) — kept for Wave #188's
           *  lazy detection. The literalized `wrapperConfig` already
           *  carries `lazy: true` for the literal-boolean case; the AST
           *  form lets the lazy inspector classify non-literal `lazy`
           *  expressions independently and warn. */
          wrapperConfigNode: any;
          node: any;
          isStream: boolean;
        }
        const serverFns: ServerFn[] = [];

        for (const node of ast.body) {
          if (node.type !== "ExportNamedDeclaration") continue;
          const decl = node.declaration;
          if (!decl) continue;

          // Plain function declarations:
          //   export function name() { ... }
          //   export async function name() { ... }
          //   export async function* name() { ... }
          //
          // These are NOT RPCs anymore. They stay in the
          // server bundle as private helpers; the synthetic SSR
          // entry's namespace iteration ignores them because they
          // lack the wrapper-attached `__zsKind` / `config.kind` tag
          // the dispatcher reads when registering procedures.
          if (decl.type === "FunctionDeclaration") continue;

          // Variable declarations: only those whose initializer is a
          // call to a wrapper marker (`procedure`/`query`/`mutation`/
          // `stream`/`subscription`) are registered.
          //
          //   export const list = query(async () => { ... });
          //   export const greet = procedure(handler, { id: "greet" });
          //
          // Helpers stay private:
          //
          //   export const helper = async () => { ... };  // skipped
          //   export const PI = 3.14;                      // skipped
          //   export const $config = { auth: "user" };     // metadata
          //                                                // (collectConfig)
          if (decl.type !== "VariableDeclaration") continue;
          for (const d of decl.declarations || []) {
            const name = d.id?.name;
            if (!name || !d.init) continue;
            // Module-level `$config` is metadata, not a server function.
            if (name === "$config") continue;

            const wrapper = matchWrapperCall(d.init, wrapperBindings);
            if (!wrapper) continue;

            // The handler is the first argument of the wrapper call.
            // Async-generator detection looks at the handler's shape;
            // explicit `stream(...)` / `subscription(...)` wrappers
            // pin `isStream: true` regardless (a stream wrapper around
            // a plain async fn is rare but legal — the runtime treats
            // the return value as the iterator).
            const handler = wrapper.handler;
            const handlerIsStream =
              !!handler &&
              ((handler.type === "FunctionExpression" && !!handler.generator) ||
                (handler.type === "ArrowFunctionExpression" && !!handler.generator));
            const isStream =
              wrapper.kind === "stream" || wrapper.kind === "subscription"
                ? true
                : handlerIsStream;

            // Literalize the wrapper's config arg (if any). Same
            // shape as `<fn>.config = { ... }` assignments: top-level
            // `input` / `output` may be Zod expressions, recorded as
            // schema markers. Anything else is rejected at the
            // manifest validation stage.
            const wrapperConfigLit = wrapper.configNode
              ? literalize(wrapper.configNode, { allowSchemaProps: true })
              : undefined;
            const wrapperConfig =
              wrapperConfigLit &&
              typeof wrapperConfigLit === "object" &&
              !Array.isArray(wrapperConfigLit)
                ? (wrapperConfigLit as Record<string, unknown>)
                : undefined;

            serverFns.push({
              name,
              markerKind: wrapper.kind,
              wrapperConfig,
              wrapperConfigNode: wrapper.configNode,
              node,
              isStream,
            });
            break; // one procedure per VariableDeclaration node
          }
        }

        if (serverFns.length === 0) return null;

        const names = serverFns.map((f) => f.name);

        // 6. Track for build report + export signature tracking
        serverFunctionMap.set(relative(root, id), new Set(names));

        // 6b. Collect per-procedure metadata for the manifest emitter.
        //     We do this once per server-env transform
        //     pass; idempotent on (filePath, exportName). The synthetic
        //     SSR entry's dispatch table is populated at module-init
        //     time from the user namespace's exports — it does NOT read
        //     this state — so we only record in the server env (where
        //     the manifest emitter runs).
        if (isServerEnv) {
          const { perFn, moduleConfig, perFnNode } = collectConfig(ast.body);
          const slug = moduleSlug(root, id);
          for (const fn of serverFns) {
            // Config merge — legacy `<fn>.config = { ... }` assignment
            // wins over the wrapper's second-arg config (legacy is
            // explicit, so it gets last-write-wins precedence). Both
            // shapes use the same authoring vocabulary; the result
            // looks identical to downstream code.
            const legacyCfg = perFn.get(fn.name);
            const cfg: Record<string, unknown> | undefined =
              fn.wrapperConfig || legacyCfg
                ? { ...(fn.wrapperConfig ?? {}), ...(legacyCfg ?? {}) }
                : undefined;

            // Wave #188 — resolve `lazy` from BOTH legacy assignment and
            // wrapper config. Legacy wins on conflict (matches the
            // overall config-merge precedence above). Non-literal
            // expressions warn and fall back to false.
            const legacyLazy = inspectLazy(perFnNode.get(fn.name));
            const wrapperLazy = inspectLazy(fn.wrapperConfigNode);
            if (legacyLazy === "non-literal" || wrapperLazy === "non-literal") {
              const rel = relative(root, id).replace(/\\/g, "/");
              const msg =
                `[zeroship:transform] ${rel} — \`${fn.name}\` has a non-literal ` +
                `\`lazy\` expression. Only \`lazy: true\` / \`lazy: false\` ` +
                `(literal booleans) are recognized; the procedure stays eager.`;
              if (typeof this.warn === "function") this.warn(msg);
              else console.warn(msg);
            }
            const lazy =
              legacyLazy === "true" ||
              legacyLazy === "false"
                ? legacyLazy === "true"
                : wrapperLazy === "true";
            const explicitKind = cfg?.kind as
              | "query"
              | "mutation"
              | "action"
              | "stream"
              | "subscription"
              | undefined;
            // Kind resolution priority:
            //   1. Explicit `kind` field on either the legacy
            //      `<fn>.config = { kind: "..." }` assignment or the
            //      wrapper's second-arg config.
            //   2. Wrapper marker name — `query`/`mutation`/`action`/
            //      `stream`/`subscription` imply a kind; the generic
            //      `procedure()` marker doesn't (defers to step 3).
            //   3. Safe default for generic wrappers: async generator →
            //      stream; otherwise mutation.
            const wrapperKind:
              | "query" | "mutation" | "action" | "stream" | "subscription" | undefined =
              fn.markerKind === "procedure" ? undefined : fn.markerKind;
            const kind = explicitKind ?? wrapperKind ?? defaultKind(fn.isStream);
            // Avoid duplicates if the transform fires twice (e.g., dev
            // server hot-reload). Replace existing record by key.
            const existingIdx = state.discoveredProcedures.findIndex(
              (p) => p.filePath === id && p.exportName === fn.name,
            );
            const record: DiscoveredProcedureRecord = {
              filePath: id,
              exportName: fn.name,
              moduleSlug: slug,
              kind,
              isStream: fn.isStream,
              config: cfg,
              moduleConfig,
              ...(lazy ? { lazy: true } : {}),
            };
            if (existingIdx >= 0) state.discoveredProcedures[existingIdx] = record;
            else state.discoveredProcedures.push(record);
          }
        }

        // Resolve wireId using the order from
        // `docs/proposals/rpc.md` §2: explicit `id` wins (looked up
        // in both the wrapper's second-arg config and the legacy
        // `<fn>.config = { ... }` assignment); default is the bare
        // export name. The production-only "missing id" check happens
        // in manifest.ts; here we just pick the same shape so register
        // and dispatch agree on the key.
        const { perFn: perFnForWireIds } = collectConfig(ast.body);
        const wireIdFor = (fn: ServerFn) => {
          const legacyId = perFnForWireIds.get(fn.name)?.id;
          const wrapperId = fn.wrapperConfig?.id;
          // Legacy assignment wins over wrapper arg (matches the
          // discovery-pass merge order — last-write-wins).
          const explicit =
            typeof legacyId === "string" && legacyId.length > 0
              ? legacyId
              : typeof wrapperId === "string" && wrapperId.length > 0
                ? wrapperId
                : undefined;
          return explicit ?? fn.name;
        };

        // Resolve kind via the same priority chain used in the
        // discovery pass (explicit .kind → wrapper marker name → name-
        // safe default). Hoisted so both server and client branches
        // can read it.
        const kindFor = (fn: ServerFn) => {
          const legacyKind = perFnForWireIds.get(fn.name)?.kind as
            | "query" | "mutation" | "action" | "stream" | "subscription" | undefined;
          const wrapperArgKind = fn.wrapperConfig?.kind as
            | "query" | "mutation" | "action" | "stream" | "subscription" | undefined;
          const explicit = legacyKind ?? wrapperArgKind;
          const wrapperKind:
            | "query" | "mutation" | "action" | "stream" | "subscription" | undefined =
            fn.markerKind === "procedure" ? undefined : fn.markerKind;
          return explicit ?? wrapperKind ?? defaultKind(fn.isStream);
        };

        const mergedConfigFor = (fn: ServerFn): Record<string, unknown> => ({
          ...(fn.wrapperConfig ?? {}),
          ...(perFnForWireIds.get(fn.name) ?? {}),
        });

        // --- SERVER ENVIRONMENT ---------------------------------------------
        //
        // No more `__zsRegister(wireId, fn)` calls — the synthetic SSR
        // entry imports each procedure directly via per-(filePath,
        // exportName) ESM imports and builds a static `_procedures` map
        // at build time from `state.discoveredProcedures`. The
        // user-module init has zero dispatch side effects.
        //
        // Attach procedure metadata onto each procedure export. The
        // patching runs at user-module-init time, BEFORE the synthetic
        // entry's `import { fn as _pN }` resolves the import binding —
        // the metadata is visible there.
        if (isServerEnv) {
          const s = new MagicString(code);

          // Monkey-patch metadata onto each procedure export. Append-only;
          // doesn't touch the original declarations, so recursive references
          // inside handler bodies keep working. Properties from
          // __makeServerProcedure (id, kind, wire) are copied onto the
          // original function via Object.defineProperty.
          const hookKeys = '["id","kind","wire"]';
          const ssrPatches = serverFns
            .map((fn) => {
              const meta = JSON.stringify({ id: wireIdFor(fn), kind: kindFor(fn), wire: "json" });
              return `__zsAttachProcedureMeta(${fn.name}, ${meta});`;
            })
            .join("\n");

          s.prepend(
            `import { __makeServerProcedure as __zsMakeServerProc } from "@zeroship/server";\n` +
            `function __zsAttachProcedureMeta(target, meta) {\n` +
            `  try {\n` +
            `    const w = __zsMakeServerProc(target, meta);\n` +
            `    for (const k of ${hookKeys}) {\n` +
            `      if (k in w) {\n` +
            `        try { Object.defineProperty(target, k, { value: w[k], enumerable: true, configurable: true, writable: true }); } catch (_) {}\n` +
            `      }\n` +
            `    }\n` +
            `  } catch (_) { /* @zeroship/server not installed — RPC dispatch still works. */ }\n` +
            `}\n`
          );
          // Emit one module-scoped registration call so the dev-bootstrap
          // can replace this module's whole wire-id set atomically. That
          // lets HMR drop stale ids when a handler is renamed or deleted.
          const registerEntries = serverFns
            .map((fn) => {
              const wid = wireIdFor(fn);
              return `${JSON.stringify(wid)}: ${fn.name}`;
            })
            .join(", ");
          const registerCall =
            `if (typeof globalThis.__registerModule === "function") ` +
            `globalThis.__registerModule(${JSON.stringify(id)}, { ${registerEntries} });`;

          s.append(
            `\n\n// zeroship: SSR hooks\n` +
            `${ssrPatches}\n` +
            `\n// zeroship: dev-bootstrap registry (harmless no-op outside dev)\n` +
            `${registerCall}\n`
          );
          return {
            code: s.toString(),
            map: s.generateMap({ source: id, includeContent: true, hires: true }),
          };
        }
        // --- CLIENT ENVIRONMENT ---------------------------------------------
        //
        // Emit stubs that call the spec wire. The whole file is replaced.
        const s = new MagicString(code);

        const stubs = serverFns.map((fn) => {
          const wid = wireIdFor(fn);
          const k = kindFor(fn);
          const cfg = mergedConfigFor(fn);
          const idempotent = cfg.idempotent === true;
          return fn.isStream
            ? clientStreamStub(fn.name, wid, k)
            : clientUnaryStub(fn.name, wid, k, idempotent);
        });

        s.overwrite(
          0,
          code.length,
          CLIENT_IMPORT_PRELUDE + CLIENT_HELPERS + "\n\n" + stubs.join("\n") + "\n",
        );
        return {
          code: s.toString(),
          map: s.generateMap({ source: id, includeContent: true, hires: true }),
        };
      },
    },
  };
}
