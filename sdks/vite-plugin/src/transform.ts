import type { Plugin } from "vite";
import { relative, extname } from "node:path";
import MagicString from "magic-string";

/**
 * Per-procedure metadata stashed at transform time. Consumed by the
 * Phase 1 manifest emitter (`src/manifest.ts`) at closeBundle to build
 * the `manifest.resources` block.
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
  kind: "query" | "mutation" | "stream" | "subscription";
  isStream: boolean;
  config?: Record<string, unknown>;
  moduleConfig?: Record<string, unknown>;
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
 * Path-based server-module predicate.
 *
 * v2 dropped the `"use server"` directive: a file is a server module
 * iff its path matches one of:
 *
 *   - `<root>/src/server.{ts,tsx,js,jsx}`     single-file flat layout
 *   - `<root>/src/server/**\/*.{ts,tsx,js,jsx}` directory layout
 *
 * Anything else — even a file that opens with `"use server"` — is
 * client code and the transform passes it through. The directive is
 * no longer a marker.
 */
export function isServerModulePath(root: string, filePath: string): boolean {
  const rel = relative(root, filePath).replace(/\\/g, "/");
  // Reject paths that escape the project root (relative starts with `..`).
  if (rel.startsWith("..")) return false;
  // Single-file layout: src/server.{ts,tsx,js,jsx}.
  if (/^src\/server\.(ts|tsx|js|jsx)$/.test(rel)) return true;
  // Directory layout: anything under src/server/.
  if (/^src\/server\//.test(rel) && /\.(ts|tsx|js|jsx)$/.test(rel)) return true;
  return false;
}

/**
 * Compute the URL-path method name for an export.
 *
 * Method names are rooted at the project root with the extension stripped:
 *   /abs/project/src/api/users.ts  →  "src/api/users"
 *
 * Per-export method is `<relPathNoExt>/<exportName>`. Paths keep forward
 * slashes so the wire (`POST /_rpc/src/api/users/getUser`) matches the
 * registry key `src/api/users/getUser` on both ends. No collisions
 * because file path is part of the key.
 */
function moduleBaseName(root: string, id: string): string {
  const rel = relative(root, id).replace(/\\/g, "/");
  const ext = extname(rel);
  return ext ? rel.slice(0, -ext.length) : rel;
}

/** Shared runtime: emitted once per client bundle. Speaks the spec wire
 *  (`/_zs/v1/<id>` with superjson `{ json, meta? }` envelope, AI-SDK Data
 *  Stream Protocol for streams). No npm deps; superjson revival is left
 *  to the consumer (rare on the bare-stub path — most apps use
 *  `@zeroship/rpc-client` directly). */
const CLIENT_HELPERS = `
async function __rpcUnary(id, input) {
  const r = await fetch("/_zs/v1/" + id, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ json: input }),
  });
  if (!r.ok) {
    let body = null;
    try { body = await r.json(); } catch {}
    const e = new Error((body && body.message) || r.statusText);
    if (body && body.code) e.code = body.code;
    if (body && body.details !== undefined) e.details = body.details;
    e.status = r.status;
    throw e;
  }
  const env = await r.json();
  return env && typeof env === "object" && "json" in env ? env.json : env;
}
async function* __rpcStream(id, input) {
  const r = await fetch("/_zs/v1/" + id, {
    method: "POST",
    headers: { "Content-Type": "application/json", "Accept": "text/event-stream" },
    body: JSON.stringify({ json: input }),
  });
  if (!r.ok) {
    let body = null;
    try { body = await r.json(); } catch {}
    const e = new Error((body && body.message) || r.statusText);
    if (body && body.code) e.code = body.code;
    e.status = r.status;
    throw e;
  }
  const reader = r.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\\n")) !== -1) {
      const line = buf.slice(0, nl); buf = buf.slice(nl + 1);
      if (!line) continue;
      const colon = line.indexOf(":"); if (colon < 0) continue;
      const tag = line.slice(0, colon);
      const data = line.slice(colon + 1);
      if (tag === "0") yield JSON.parse(data);
      else if (tag === "2") { const arr = JSON.parse(data); for (const v of arr) yield v; }
      else if (tag === "e") { const env = JSON.parse(data); const e = new Error(env.message || "stream error"); if (env.code) e.code = env.code; if (env.details !== undefined) e.details = env.details; throw e; }
      else if (tag === "d") return;
    }
  }
}
`.trim();

/**
 * Slug a module file path into a stable id segment.
 *
 *   `src/server/todos.ts` → `src-server-todos`
 *
 * NOTE: As of the Phase 1 follow-up, the slug is NOT used in wireId
 * derivation — the default wireId is just the bare `<exportName>`.
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
 * Infer the procedure kind from the function's name.
 *
 *   /^(get|list|find|search|count|read|fetch)/ → "query"
 *   else                                       → "mutation"
 *
 * Async generators get `kind: "stream"` regardless of name. An explicit
 * `.config = { kind: "..." }` overrides everything.
 */
function inferKind(
  name: string,
  isStream: boolean,
): "query" | "mutation" | "stream" | "subscription" {
  if (isStream) return "stream";
  if (/^(get|list|find|search|count|read|fetch)[A-Z_]?/.test(name)) {
    return "query";
  }
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
 * Walk module body, collecting `<fnName>.config = { ... }` and
 * `export const $config = { ... }` assignments. Returns:
 *
 *   - perFn:    Map<exportName, configLiteral>
 *   - moduleConfig: literal of `$config` or undefined
 */
function collectConfig(astBody: any[]): {
  perFn: Map<string, Record<string, unknown>>;
  moduleConfig: Record<string, unknown> | undefined;
} {
  const perFn = new Map<string, Record<string, unknown>>();
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
      const lit = literalize(node.expression.right, { allowSchemaProps: true });
      if (lit && typeof lit === "object" && !Array.isArray(lit)) {
        perFn.set(fnName, lit as Record<string, unknown>);
      }
    }
  }
  return { perFn, moduleConfig };
}

/** Client stub for a non-streaming export */
function clientUnaryStub(name: string, methodName: string): string {
  return `export const ${name} = (...args) => __rpcUnary(${JSON.stringify(methodName)}, args);`;
}

/** Client stub for a streaming (async generator) export */
function clientStreamStub(name: string, methodName: string): string {
  return `export const ${name} = (...args) => __rpcStream(${JSON.stringify(methodName)}, args);`;
}

export function transformPlugin(_rpcEndpoint: string, state: TransformState): Plugin {
  const { serverFunctionMap } = state;
  let root = "";

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

        // 1. Server-module gate — purely path-based.
        //    Files at `src/server.{ts,tsx,js,jsx}` or anywhere under
        //    `src/server/` are server modules; everything else is client
        //    code and the transform passes it through. The legacy
        //    `"use server"` directive is no longer accepted.
        if (!isServerModulePath(root, id)) return null;

        // 2. Parse AST with Rolldown's built-in parser.
        const isTsx = id.endsWith(".tsx") || id.endsWith(".jsx");
        const ast = this.parse(code, { lang: isTsx ? "tsx" : "ts" });

        // 3. Find server functions: every async-or-not function /
        //    arrow / generator export at module scope. Because the
        //    file is wholly server-side (path convention), any
        //    function export is a server function — there is no
        //    "mixed" file shape in v2.
        interface ServerFn {
          name: string;
          node: any;
          isStream: boolean;
        }
        const serverFns: ServerFn[] = [];

        for (const node of ast.body) {
          if (node.type !== "ExportNamedDeclaration") continue;
          const decl = node.declaration;
          if (!decl) continue;

          // export function name() { ... }
          // export async function name() { ... }
          // export async function* name() { ... }
          if (decl.type === "FunctionDeclaration" && decl.id?.name) {
            const name = decl.id.name;
            serverFns.push({ name, node, isStream: !!decl.generator });
          }

          // export const name = () => { ... }
          // export const name = async function() { ... }
          // export const name = async function*() { ... }
          if (decl.type === "VariableDeclaration") {
            for (const d of decl.declarations || []) {
              const name = d.id?.name;
              if (!name || !d.init) continue;
              // Skip the module-level `$config` declaration — it is
              // metadata, not a server function. The manifest emitter
              // reads it via collectConfig().
              if (name === "$config") continue;
              // Only treat as a server function if the initializer is
              // actually a function (Arrow/Function/AsyncFunction).
              const isFn =
                d.init.type === "ArrowFunctionExpression" ||
                d.init.type === "FunctionExpression";
              if (!isFn) continue;
              const isStream =
                (d.init.type === "FunctionExpression" && !!d.init.generator) ||
                (d.init.type === "ArrowFunctionExpression" && !!d.init.generator);
              serverFns.push({ name, node, isStream });
              break; // one removal per VariableDeclaration node
            }
          }
        }

        if (serverFns.length === 0) return null;

        const names = serverFns.map((f) => f.name);
        const modPath = moduleBaseName(root, id);

        // 6. Track for build report + export signature tracking
        serverFunctionMap.set(relative(root, id), new Set(names));

        // 6b. Phase 1 RPC v2: collect per-procedure metadata for the
        //     manifest emitter. We do this once per server-env transform
        //     pass; idempotent on (filePath, exportName). The synthetic
        //     SSR entry's dispatch table is populated at module-init
        //     time from the user namespace's exports — it does NOT read
        //     this state — so we only record in the server env (where
        //     the manifest emitter runs).
        if (isServerEnv) {
          const { perFn, moduleConfig } = collectConfig(ast.body);
          const slug = moduleSlug(root, id);
          for (const fn of serverFns) {
            const cfg = perFn.get(fn.name);
            const explicitKind = cfg?.kind as
              | "query"
              | "mutation"
              | "stream"
              | "subscription"
              | undefined;
            const kind = explicitKind ?? inferKind(fn.name, fn.isStream);
            // Avoid duplicates if the transform fires twice (e.g., dev
            // server hot-reload). Replace existing record by key.
            const existingIdx = state.discoveredProcedures.findIndex(
              (p) => p.filePath === id && p.exportName === fn.name,
            );
            const record = {
              filePath: id,
              exportName: fn.name,
              moduleSlug: slug,
              kind,
              isStream: fn.isStream,
              config: cfg,
              moduleConfig,
            };
            if (existingIdx >= 0) state.discoveredProcedures[existingIdx] = record;
            else state.discoveredProcedures.push(record);
          }
        }

        // Resolve wireId per spec §2: explicit fn.config.id wins; default
        // is bare exportName. Production-mode "missing id" check happens
        // in manifest.ts; here we just pick the same shape so register
        // and dispatch agree on the key.
        const { perFn: perFnForWireIds } = collectConfig(ast.body);
        const wireIdFor = (fn: { name: string }) => {
          const explicit = perFnForWireIds.get(fn.name)?.id;
          return typeof explicit === "string" && explicit.length > 0
            ? explicit
            : fn.name;
        };

        // --- SERVER ENVIRONMENT ---------------------------------------------
        //
        // No more `__zsRegister(wireId, fn)` calls — the synthetic SSR
        // entry imports each procedure directly via per-(filePath,
        // exportName) ESM imports and builds a static `_procedures` map
        // at build time from `state.discoveredProcedures`. The
        // user-module init has zero dispatch side effects.
        //
        // We still monkey-patch SSR hooks (.useQuery, .prefetch, .id,
        // .kind, .queryKey) onto each procedure export so React
        // components rendering server-side find them on import. The
        // patching runs at user-module-init time, BEFORE the synthetic
        // entry's `import { fn as _pN }` resolves the import binding —
        // the patches are visible there.
        if (isServerEnv) {
          // Resolve kind for SSR-side hook attachment.
          const { perFn: perFnForKind } = collectConfig(ast.body);
          const kindFor = (fn: { name: string; isStream: boolean }) => {
            const explicit = perFnForKind.get(fn.name)?.kind as
              | "query" | "mutation" | "stream" | "subscription" | undefined;
            return explicit ?? inferKind(fn.name, fn.isStream);
          };

          const s = new MagicString(code);

          // Monkey-patch SSR hooks onto each procedure export. Append-only;
          // doesn't touch the original declarations, so recursive references
          // inside handler bodies keep working. Properties from
          // __makeServerProcedure (id, kind, queryKey, useQuery,
          // useSuspenseQuery, prefetch, useMutation, useStream,
          // useSubscription) are copied onto the original function via
          // Object.defineProperty — components importing the export see
          // them attached.
          const hookKeys = '["id","kind","queryKey","useQuery","useSuspenseQuery","prefetch","useMutation","useStream","useSubscription"]';
          const ssrPatches = serverFns
            .map((fn) => {
              const meta = JSON.stringify({ id: wireIdFor(fn), kind: kindFor(fn) });
              return `__zsAttachHooks(${fn.name}, ${meta});`;
            })
            .join("\n");

          s.prepend(
            `import { __makeServerProcedure as __zsMakeServerProc } from "@zeroship/server";\n` +
            `function __zsAttachHooks(target, meta) {\n` +
            `  try {\n` +
            `    const w = __zsMakeServerProc(target, meta);\n` +
            `    for (const k of ${hookKeys}) {\n` +
            `      if (k in w) {\n` +
            `        try { Object.defineProperty(target, k, { value: w[k], enumerable: true, configurable: true, writable: true }); } catch (_) {}\n` +
            `      }\n` +
            `    }\n` +
            `  } catch (_) { /* @zeroship/server not installed — SSR hooks unavailable. RPC dispatch still works. */ }\n` +
            `}\n`
          );
          s.append(
            `\n\n// zeroship: SSR hooks\n` +
            `${ssrPatches}\n`
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
          return fn.isStream
            ? clientStreamStub(fn.name, wid)
            : clientUnaryStub(fn.name, wid);
        });

        s.overwrite(0, code.length, CLIENT_HELPERS + "\n\n" + stubs.join("\n") + "\n");
        return {
          code: s.toString(),
          map: s.generateMap({ source: id, includeContent: true, hires: true }),
        };
      },
    },
  };
}
