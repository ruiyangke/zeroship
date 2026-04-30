// sdks/vite-plugin/src/rpc-registry.ts
//
// Closure-private RPC registry virtual module.
//
// Replaces the old `globalThis.__zsRegistry` / `globalThis.__register` leak,
// which exposed the registry to every npm package in the SSR bundle. After
// build, no `__zsRegistry`, `__register`, or `__zsBufferModule__` symbol
// exists anywhere on globalThis — the registry lives as a top-level
// (rolldown-mangled) `const` inside the bundled module's flat scope.
//
// Two virtual modules are exposed by `rpcRegistryPlugin`:
//
//   virtual:zeroship/_rpc-registry  — ESM module declaring the registry,
//                                     `_zsRegister(name, fn)` and
//                                     `dispatch(method, args)`. The
//                                     `_registry` Map is **not** exported,
//                                     so user code can't read or mutate it.
//
//   virtual:zeroship/_server-entry  — synthetic SSR entry that imports the
//                                     user's entry, re-exports its bindings,
//                                     and provides our own `default.fetch`
//                                     + `dispatchRpc` exports. Used as
//                                     `build.ssr` so the SSR sub-build
//                                     starts from a module we own.
//
// The transform plugin emits
//   `import { _zsRegister as __zsRegister } from "virtual:zeroship/_rpc-registry"`
// in every transformed server module. When rolldown bundles the SSR build,
// it merges the registry virtual module's source into the same flat scope
// as the user code; the registration calls run as plain function calls on
// a module-private Map.

import type { Plugin } from "vite";

// ── Public IDs ─────────────────────────────────────────────────────────────

/** Public specifier for the RPC registry virtual module. */
export const RPC_REGISTRY_VIRTUAL_ID = "virtual:zeroship/_rpc-registry";
/** Internal (\0-prefixed) id Vite uses for the same module. */
export const RPC_REGISTRY_RESOLVED_ID = "\0" + RPC_REGISTRY_VIRTUAL_ID;

/** Public specifier for the synthetic server entry. */
export const SERVER_ENTRY_VIRTUAL_ID = "virtual:zeroship/_server-entry";
/** Internal (\0-prefixed) id Vite uses for the synthetic entry. */
export const SERVER_ENTRY_RESOLVED_ID = "\0" + SERVER_ENTRY_VIRTUAL_ID;

// ── Registry source ────────────────────────────────────────────────────────

/**
 * ESM source of the closure-private registry module.
 *
 * The `_registry` Map is module-scoped (NOT exported). Once the rolldown
 * bundler merges this into the flat-scope output, the binding becomes a
 * top-level `const` with a rolldown-mangled name, unreachable from
 * user-bundled npm packages.
 *
 * Exports:
 *   _zsRegister(name, fn)   — register a server function under a name.
 *   dispatch(method, args)  — invoke a registered function. On miss
 *                             throws `Object.assign(new Error("Method not found: " + name),
 *                                                  { status: 404 })` so the runtime
 *                             can map err.status -> HTTP status.
 *
 * NOT exported (intentional):
 *   _registry — closure-private. No way to enumerate, replace, or delete
 *               registered methods from outside this module.
 *
 * Validation:
 *   `dispatch` reads `fn.config.input` and `fn.config.output` at call
 *   time. When `fn.config.input` is a Zod schema (anything with a
 *   `.parse()` method), the input is validated against it; failures
 *   throw an `INVALID_ARGUMENT` error (status 400) carrying the Zod
 *   issues array. This is the Level 1 trust model — procedures without
 *   declared schemas pass arguments through unchecked.
 *
 *   Output validation runs only when `process.env.NODE_ENV !==
 *   "production"`. It's a cheap dev-time correctness check; the hot
 *   path skips it in production.
 */
export const RPC_REGISTRY_SOURCE = `// virtual:zeroship/_rpc-registry — closure-private
const _registry = new Map();

export function _zsRegister(name, fn) {
  _registry.set(name, fn);
}

function _isParseable(s) {
  return s != null && typeof s === "object" && typeof s.parse === "function";
}

function _zodIssues(err) {
  // ZodError carries either err.issues (modern) or err.errors (legacy).
  // Either way we forward the raw structured array — clients that want
  // structured errors get them, raw error message readers still see
  // err.message.
  if (err && Array.isArray(err.issues)) return err.issues;
  if (err && Array.isArray(err.errors)) return err.errors;
  return undefined;
}

function _isZodError(err) {
  if (!err || typeof err !== "object") return false;
  if (err.name === "ZodError") return true;
  // Some Zod versions don't set the name; detect by issues/errors shape.
  return Array.isArray(err.issues) || Array.isArray(err.errors);
}

// Detect a Zod string schema (for AI-SDK \`0:\` text-part wire framing).
// Zod schemas carry a \`_def.typeName === "ZodString"\` tag in v3+; some
// builds expose \`.def.type === "string"\` (Zod v4) instead. We check both.
// Anything that doesn't match returns false → the encoder defaults to
// \`2:\` (object lane) for unknown shapes.
function _isZodStringSchema(s) {
  if (!s || typeof s !== "object") return false;
  const def = s._def || s.def;
  if (!def) return false;
  if (def.typeName === "ZodString") return true;
  if (def.type === "string") return true;
  return false;
}

function _isAsyncIterator(x) {
  return (
    x != null &&
    typeof x === "object" &&
    typeof x[Symbol.asyncIterator] === "function" &&
    typeof x.next === "function"
  );
}

export async function dispatch(methodName, args, opts) {
  const fn = _registry.get(methodName);
  if (typeof fn !== "function") {
    throw Object.assign(new Error("Method not found: " + methodName), { status: 404 });
  }
  const argv = Array.isArray(args) ? args.slice() : [];
  const cfg = fn.config;
  const inputSchema  = cfg ? cfg.input  : undefined;
  const outputSchema = cfg ? cfg.output : undefined;
  const wantStream = !!(opts && opts.wantStream);

  // Input validation. When declared, validates argv[0]; the parsed
  // (possibly transformed) value is substituted back so transforms
  // like .transform() and .default() reach the handler.
  if (_isParseable(inputSchema)) {
    try {
      argv[0] = inputSchema.parse(argv[0]);
    } catch (e) {
      if (_isZodError(e)) {
        throw Object.assign(new Error("Invalid input"), {
          status: 400,
          code: "INVALID_ARGUMENT",
          details: { issues: _zodIssues(e) },
        });
      }
      throw e;
    }
  }

  const result = await fn.apply(null, argv);

  // Streaming path. When the caller (the synthetic entry's \`_zsFetch\`
  // or the runtime kernel's fast path) signals \`wantStream\` AND the
  // handler returned an async iterator, we tag the iterator with
  // \`__zsOutputIsString\` (read by both sides) and skip output
  // validation — the schema describes a single yield, not the
  // iterator-as-a-whole.
  if (wantStream && _isAsyncIterator(result)) {
    if (_isZodStringSchema(outputSchema)) {
      try {
        Object.defineProperty(result, "__zsOutputIsString", {
          value: true,
          enumerable: false,
          configurable: true,
          writable: true,
        });
      } catch (_e) {
        // Defensive: some iterator implementations are frozen. Fall
        // back to a plain assignment which the encoder still reads.
        try { result.__zsOutputIsString = true; } catch (_e2) { /* ignore */ }
      }
    }
    return result;
  }

  // Output validation runs in dev only. Production hot-path skips it.
  // Output validators throw on mismatch — that's an INTERNAL error
  // (the handler returned the wrong shape), distinct from
  // INVALID_ARGUMENT which is the caller's fault.
  const isProd =
    typeof process !== "undefined" &&
    process &&
    process.env &&
    process.env.NODE_ENV === "production";
  if (!isProd && _isParseable(outputSchema)) {
    try {
      outputSchema.parse(result);
    } catch (e) {
      if (_isZodError(e)) {
        throw Object.assign(new Error("Invalid handler output"), {
          status: 500,
          code: "INTERNAL",
          details: { issues: _zodIssues(e) },
        });
      }
      throw e;
    }
  }

  return result;
}
`;

// ── Synthetic entry source ─────────────────────────────────────────────────

/**
 * Build the source of the synthetic server entry.
 *
 * Behavior:
 *   - Imports the user's entry as a namespace AND re-exports it.
 *   - Imports `dispatch` from the registry (re-exported as `dispatchRpc` so
 *     the runtime's BOOTSTRAP_JS finds it on `user.dispatchRpc`).
 *   - Provides `default.fetch` that handles `/_rpc/*` via the registry,
 *     and falls through to the user's `default.fetch` for everything else.
 *
 * The user's `default` is imported via the namespace (`_zsUser.default`)
 * so we can detect their `fetch` at module-init time. Per AGENTS.md, the
 * platform contract requires `default` to be `{ fetch }`-shaped on the
 * SSR side — function-shaped defaults aren't supported.
 *
 * @param opts.userEntryRel  Specifier the synthetic entry should use to
 *                           import the user module. Posix-style relative
 *                           path (or absolute import-resolvable id).
 */
export function buildServerEntrySource(opts: { userEntryRel: string }): string {
  const userImport = JSON.stringify(opts.userEntryRel);
  const registryImport = JSON.stringify(RPC_REGISTRY_VIRTUAL_ID);
  return `// virtual:zeroship/_server-entry — synthetic SSR entry
import * as _zsUser from ${userImport};
export * from ${userImport};
import { dispatch as _zsDispatch } from ${registryImport};

// dispatchRpc — exported so the runtime kernel's fast path picks it up
// (\`user.dispatchRpc(method, args)\`). Just rethrows; the kernel's
// errorResponse serializer carries \`code\` / \`details\` / \`retryable\`
// alongside the existing \`message\` / \`name\` / \`status\` fields, so
// structured errors (INVALID_ARGUMENT, INTERNAL) reach the wire without
// any local Response-building. Earlier revisions had to wrap manually
// because the kernel dropped everything except message/name.
//
// We always pass \`wantStream: true\` here. The registry only honors it
// when the handler actually returns an async iterator; for unary
// procedures it's a no-op. This lets the runtime kernel's
// \`sseFromAsyncGen\` read \`result.__zsOutputIsString\` to pick the
// AI-SDK \`0:\` (text) vs \`2:\` (object) lane.
export async function dispatchRpc(methodName, args) {
  return await _zsDispatch(methodName, args, { wantStream: true });
}

const _userDefault = (_zsUser && _zsUser.default && typeof _zsUser.default === "object")
  ? _zsUser.default : null;
const _userFetch = _userDefault && typeof _userDefault.fetch === "function" ? _userDefault.fetch : null;

async function _zsFetch(request) {
  const url = new URL(request.url);
  if (url.pathname.startsWith("/_rpc/")) {
    const methodName = url.pathname.slice("/_rpc/".length);
    let args = [];
    const text = await request.text();
    if (text) {
      try {
        const parsed = JSON.parse(text);
        if (Array.isArray(parsed)) args = parsed;
        else throw new Error("RPC body must be a JSON array");
      } catch (e) {
        return new Response(
          JSON.stringify({ message: e?.message ?? String(e), name: "Error" }),
          { status: 400, headers: { "content-type": "application/json" } },
        );
      }
    }

    try {
      const result = await _zsDispatch(methodName, args, { wantStream: true });

      if (
        result != null &&
        typeof result === "object" &&
        typeof result[Symbol.asyncIterator] === "function" &&
        typeof result.next === "function"
      ) {
        // Vercel AI-SDK Data Stream Protocol — line-prefixed framing:
        //   0:"text"\\n          string yields
        //   2:[<json>]\\n        object yields
        //   e:{...}\\n           structured error envelope (zeroship ext)
        //   d:{}\\n              done
        // The synthetic entry's slow path mirrors the runtime kernel's
        // \`sseFromAsyncGen\` byte-for-byte. The \`__zsOutputIsString\`
        // tag (set by registry dispatch when \`fn.config.output\` is a
        // Zod string schema) forces every yield to the \`0:\` lane.
        const outputIsString = !!result.__zsOutputIsString;
        const encoder = new TextEncoder();
        const body = new ReadableStream({
          async start(controller) {
            try {
              while (true) {
                const step = await result.next();
                if (step.done) {
                  controller.enqueue(encoder.encode("d:{}\\n"));
                  break;
                }
                const v = step.value;
                if (outputIsString || typeof v === "string") {
                  controller.enqueue(encoder.encode("0:" + JSON.stringify(String(v)) + "\\n"));
                } else {
                  controller.enqueue(encoder.encode("2:[" + JSON.stringify(v) + "]\\n"));
                }
              }
            } catch (e) {
              const env = {
                message: (e && e.message) || String(e),
                name:    (e && e.name)    || "Error",
              };
              if (e && typeof e.code === "string") env.code = e.code;
              if (e && e.details !== undefined)    env.details = e.details;
              if (e && typeof e.retryable === "boolean") env.retryable = e.retryable;
              controller.enqueue(encoder.encode("e:" + JSON.stringify(env) + "\\n"));
              controller.enqueue(encoder.encode("d:{}\\n"));
            } finally {
              controller.close();
            }
          },
        });
        return new Response(body, {
          status: 200,
          headers: {
            "content-type": "text/event-stream",
            "cache-control": "no-cache, no-transform",
            "x-accel-buffering": "no",
          },
        });
      }

      if (result instanceof Response) return result;

      return new Response(
        JSON.stringify(result === undefined ? null : result),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    } catch (err) {
      const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600)
        ? err.status : 500;
      const body = {
        message: err?.message ?? String(err),
        name: err?.name ?? "Error",
      };
      // Carry the structured error envelope when dispatch attached
      // INVALID_ARGUMENT / INTERNAL metadata (Zod validation path).
      if (err && typeof err.code === "string") body.code = err.code;
      if (err && err.details !== undefined) body.details = err.details;
      return new Response(
        JSON.stringify(body),
        { status, headers: { "content-type": "application/json" } },
      );
    }
  }

  if (_userFetch) return _userFetch.call(_userDefault, request);
  return new Response("Not Found", { status: 404 });
}

export default { fetch: _zsFetch };
`;
}

// ── Vite plugin ────────────────────────────────────────────────────────────

/**
 * Vite plugin that resolves and loads the two virtual modules.
 *
 * `enforce: "pre"` — run before user-installed plugins so the virtual ids
 * never leak to the file resolver. The plugin owns these specifiers in
 * full: any `virtual:zeroship/_rpc-registry` import in the graph (emitted
 * by `transformPlugin`) routes through `load` here.
 *
 * @param opts.userEntryRel  Path the synthetic entry should import from.
 *                           Required only when the synthetic entry is
 *                           used (i.e. when `build.ssr ===
 *                           SERVER_ENTRY_VIRTUAL_ID`). For dev-mode use
 *                           where only the registry virtual is needed,
 *                           `userEntryRel` is unused but still required
 *                           by the type — pass any string.
 */
export function rpcRegistryPlugin(opts: { userEntryRel: string }): Plugin {
  return {
    name: "zeroship:rpc-registry",
    enforce: "pre",
    resolveId(id: string) {
      if (id === RPC_REGISTRY_VIRTUAL_ID) return RPC_REGISTRY_RESOLVED_ID;
      if (id === SERVER_ENTRY_VIRTUAL_ID) return SERVER_ENTRY_RESOLVED_ID;
      return null;
    },
    load(id: string) {
      if (id === RPC_REGISTRY_RESOLVED_ID) return RPC_REGISTRY_SOURCE;
      if (id === SERVER_ENTRY_RESOLVED_ID) {
        return buildServerEntrySource({ userEntryRel: opts.userEntryRel });
      }
      return null;
    },
  };
}
