// sdks/vite-plugin/src/rpc-registry.ts
//
// Synthetic SSR entry generator.
//
// The plugin emits one virtual module:
//
//   virtual:zeroship/_server-entry  — synthetic SSR entry that imports the
//                                     user's entry, builds a static
//                                     `_procedures` map from procedures
//                                     discovered at transform time, and
//                                     exports `default.{fetch, rpc}` —
//                                     mirroring WinterCG's `default.fetch`.
//
// Wire shape — `default`:
//
//   fetch(request)         WinterCG-symmetric HTTP handler. /_zs/v1/<id>
//                          requests dispatch through `rpc`; everything
//                          else falls through to the user's own
//                          `default.fetch` (when present), or 404s.
//   rpc(name, input, ctx)  Standalone kernel entry point — symmetric
//                          to `default.fetch`. Looks up the wireId in
//                          the dispatch table built from the user
//                          module's namespace exports at module-init
//                          time, validates `fn.config.input` (Zod),
//                          runs the handler, optionally validates
//                          `fn.config.output` (dev only). The kernel
//                          calls this directly when the URL matches
//                          /_zs/v1/<id>, bypassing Request/URL
//                          construction entirely. Returns either the
//                          handler's sync value, a Promise, or an
//                          AsyncIterator (stream — tagged with
//                          `__zsOutputIsString` when output schema is a
//                          Zod string so the wire encoder picks the `0:`
//                          lane). NOT `async` — sync handlers stay sync
//                          to avoid the per-request microtask tax.
//
// No virtual registry module, no `_zsRegister`, no side effects in user
// modules — the dispatch table is statically built by the plugin from
// `state.discoveredProcedures` at `load()` time.

import type { Plugin } from "vite";
import type { TransformState } from "./transform.js";
import type { ServerBinding } from "./server-graph.js";

// ── Public IDs ─────────────────────────────────────────────────────────────

/** Public specifier for the synthetic server entry. */
export const SERVER_ENTRY_VIRTUAL_ID = "virtual:zeroship/_server-entry";
/** Internal (\0-prefixed) id Vite uses for the synthetic entry. */
export const SERVER_ENTRY_RESOLVED_ID = "\0" + SERVER_ENTRY_VIRTUAL_ID;

// ── Synthetic entry source ─────────────────────────────────────────────────

/**
 * Pick the wireId for a procedure. Mirrors `pickWireId()` in manifest.ts:
 * explicit `fn.config.id` (when string and non-empty) wins; default is
 * the bare export name. Kept in sync because the synthetic entry's
 * dispatch table must use the same key the manifest emitter advertises.
 */
export function pickEntryWireId(p: {
  exportName: string;
  config?: Record<string, unknown>;
}): string {
  const explicit = p.config?.id;
  if (typeof explicit === "string" && explicit.length > 0) return explicit;
  return p.exportName;
}

/**
 * Build the source of the synthetic server entry.
 *
 * The dispatch table (`_procedures`) is populated at module-init time
 * by iterating the user module's namespace exports — every callable
 * non-`default` export is registered under `fn.config.id` (when
 * explicit) or its export name. There are no per-procedure static
 * imports; the user module's namespace is the source of truth.
 *
 * Emits `default.{fetch, rpc}` — symmetric to WinterCG's
 * `default.fetch`. No runtime registration, no virtual registry
 * module, no side effects in user modules.
 *
 * @param opts.userEntryRel  Specifier the synthetic entry should use to
 *                           import the user module (its default export
 *                           and named procedure exports surface via
 *                           `_zsUser.*`).
 */
export function buildServerEntrySource(opts: {
  userEntryRel: string;
  /** Explicit server-binding map. When provided, the synthetic entry
   *  emits per-target imports and a static `_procedures` object
   *  literal. Falls back to namespace-walk registration when omitted. */
  bindings?: Map<string, ServerBinding>;
}): string {
  const userImport = JSON.stringify(opts.userEntryRel);

  // Static dispatch table fed by walkClientEntry().
  if (opts.bindings && opts.bindings.size > 0) {
    return buildPhase2Entry(userImport, opts.bindings);
  }

  return `// virtual:zeroship/_server-entry — auto-generated synthetic entry
// Procedures are discovered at module-init time from the user module's
// own namespace exports. Each callable export (other than \`default\`)
// is registered under its export name, with \`fn.config.id\` overriding
// when present.

import * as _zsUser from ${userImport};

const _procedures = {};
for (const _k of Object.keys(_zsUser)) {
  if (_k === "default") continue;
  const _v = _zsUser[_k];
  if (typeof _v !== "function") continue;
  const _id = (_v.config && typeof _v.config.id === "string" && _v.config.id) || _k;
  _procedures[_id] = _v;
}

const _userDefault = (_zsUser && _zsUser.default && typeof _zsUser.default === "object")
  ? _zsUser.default : null;
const _userFetch = _userDefault && typeof _userDefault.fetch === "function"
  ? _userDefault.fetch : null;

function _zsErrResponse(status, code, message, details) {
  const body = { message, name: "Error", code };
  if (details !== undefined) body.details = details;
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function _isAsyncIterator(x) {
  return (
    x != null &&
    typeof x === "object" &&
    typeof x[Symbol.asyncIterator] === "function" &&
    typeof x.next === "function"
  );
}

function _isParseable(s) {
  return s != null && typeof s === "object" && typeof s.parse === "function";
}

function _zodIssues(err) {
  if (err && Array.isArray(err.issues)) return err.issues;
  if (err && Array.isArray(err.errors)) return err.errors;
  return [];
}

// Detect a Zod string schema (for AI-SDK \`0:\` text-part wire framing).
function _isZodStringSchema(s) {
  if (!s || typeof s !== "object") return false;
  const def = s._def || s.def;
  if (!def) return false;
  if (def.typeName === "ZodString") return true;
  if (def.type === "string") return true;
  return false;
}

// _zsRpc(name, input, ctx) — the RPC dispatcher. Symmetric to fetch:
// the kernel's HTTP fast path (default.rpc dispatch) calls this with
// (wireId, input, ctx); the slow-path fall-through (_zsFetch →
// _zsRpcAndRespond) calls it with the same shape; WS subscriptions
// and tests do the same.
//
// NOT \`async\`: an async function would always return a Promise and
// always cost one microtask, even for sync handlers. We keep this sync
// and only attach \`.then\` when the user handler returns a thenable.
// On the bench (sync ping/fib), this saves the per-request microtask
// checkpoint.
//
// B3 capability marker. Around the user handler we set the runtime's
// thread-local CURRENT_KIND via the native __zsEnterKind / __zsExitKind
// callbacks. The Rust-side db write callbacks and fetch callback read
// this marker to refuse capability-violating ops (query → no write,
// mutation → no fetch). The token roundtrip is a stack so nested calls
// (action → runMutation → mutation handler) restore the outer kind on
// inner exit. Falls back to no-op when the natives aren't installed
// (legacy embeddings / dev-bootstrap).
//
// T1 follow-up — Postgres-level auto-tx wrapping. When kind is
// "query" or "mutation" AND plugin-db is installed (i.e.
// globalThis.__zsBeginAutoTx is present), wrap the handler call in a
// READ ONLY / SERIALIZABLE transaction via the natives. The capability
// gate is the primary enforcement; auto-tx is defense-in-depth: even if
// a query handler smuggles past the gate (e.g. an SDK escape hatch
// that calls raw SQL), Postgres refuses with SQLSTATE 25006. Action,
// stream, subscription, and unknown kinds bypass the wrapper (stream/
// subscription would hold a tx open across cell-aborting boundaries;
// action is by design able to touch external IO so it'd starve the
// pool). Begin returns 0 for unwrapped kinds — End is then a noop too.
function _zsRpc(name, input, ctx) {
  const fn = _procedures[name];
  if (typeof fn !== "function") {
    throw Object.assign(new Error("Method not found: " + name), {
      status: 404,
      code: "NOT_FOUND",
    });
  }

  let validated = input;
  const cfg = fn.config;
  if (cfg && _isParseable(cfg.input)) {
    try {
      validated = cfg.input.parse(input);
    } catch (e) {
      throw Object.assign(new Error("Invalid input"), {
        status: 400,
        code: "INVALID_ARGUMENT",
        details: { issues: _zodIssues(e) },
      });
    }
  }

  // Resolve the procedure kind cheaply. fn.config.kind is the canonical
  // source (set by the typed wrapper or by inferKind() at build time);
  // fn.__zsKind is the non-enumerable fallback the wrappers attach so
  // capability dispatch survives even when config is absent.
  const kind = (cfg && typeof cfg.kind === "string" && cfg.kind) ||
               (typeof fn.__zsKind === "string" && fn.__zsKind) ||
               undefined;
  const ek = (typeof globalThis !== "undefined") ? globalThis.__zsEnterKind : undefined;
  const xk = (typeof globalThis !== "undefined") ? globalThis.__zsExitKind : undefined;
  const tok = (kind && typeof ek === "function") ? ek(kind) : -1;

  // Auto-tx path: query/mutation when plugin-db is installed. Async by
  // construction (begin/commit are awaited). Everything else stays on
  // the sync-eligible fast path below.
  const bt = (typeof globalThis !== "undefined") ? globalThis.__zsBeginAutoTx : undefined;
  const et = (typeof globalThis !== "undefined") ? globalThis.__zsEndAutoTx : undefined;
  const wantsAutoTx = (kind === "query" || kind === "mutation") &&
                      typeof bt === "function" && typeof et === "function";
  if (wantsAutoTx) {
    return _zsRpcWithAutoTx(fn, validated, ctx, cfg, kind, tok, xk, bt, et);
  }

  try {
    const out = fn(validated, ctx);
    if (out && typeof out.then === "function") {
      // Async path — pop the marker after the Promise settles, win OR lose,
      // so a rejected handler still releases CURRENT_KIND. The .then chain
      // returns the same value/error the user produced.
      return out.then(
        (v) => { if (tok >= 0 && typeof xk === "function") xk(tok); return _zsRpcPost(v, cfg); },
        (e) => { if (tok >= 0 && typeof xk === "function") xk(tok); throw e; },
      );
    }
    const post = _zsRpcPost(out, cfg);
    if (tok >= 0 && typeof xk === "function") xk(tok);
    return post;
  } catch (e) {
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw e;
  }
}

// Async auto-tx envelope: BEGIN → handler → COMMIT (success) or
// ROLLBACK (failure). Errors from the handler propagate after the
// rollback completes; commit failures surface as the caller's error
// (data integrity wins). CURRENT_KIND is popped exactly once on every
// exit path (begin throw, handler throw, commit throw, success).
async function _zsRpcWithAutoTx(fn, input, ctx, cfg, _kind, tok, xk, bt, et) {
  // Begin returns 0 for unwrapped/no-op cases (kind doesn't match, or
  // a user-driven db.transaction is already open). End on 0 is a noop.
  let token = 0;
  try {
    token = await bt(_kind);
  } catch (beginErr) {
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw beginErr;
  }
  // Phase 1 — run the handler. Rollback on throw.
  let out;
  try {
    out = fn(input, ctx);
    if (out && typeof out.then === "function") out = await out;
  } catch (handlerErr) {
    try { await et(token, false); } catch (_rb) { /* swallowed */ }
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw handlerErr;
  }
  // Phase 2 — commit. Commit failure becomes the caller-visible error.
  try {
    await et(token, true);
  } catch (commitErr) {
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw commitErr;
  }
  // Output validation runs in dev only inside _zsRpcPost; happens after
  // commit so the tx isn't held open across the (synchronous) parse.
  const post = _zsRpcPost(out, cfg);
  if (tok >= 0 && typeof xk === "function") xk(tok);
  return post;
}

// Output-validation + stream-tag tail. Runs after either sync return
// or Promise resolve. Kept tiny — output validation is dev-only.
function _zsRpcPost(result, cfg) {
  // Async iterator → tag for stream encoding when output schema is a
  // Zod string. The encoder (slow path here, runtime fast path
  // elsewhere) reads __zsOutputIsString to decide between AI-SDK \`0:\`
  // (text) and \`2:\` (object) lanes.
  if (_isAsyncIterator(result)) {
    if (cfg && _isZodStringSchema(cfg.output)) {
      try { result.__zsOutputIsString = true; } catch (_e) { /* frozen iterator */ }
    }
    return result;
  }

  // Output validation runs in dev only. Production hot-path skips.
  // Default is NOT dev — only NODE_ENV === "development" opts in.
  const isDev =
    typeof process !== "undefined" &&
    process &&
    process.env &&
    process.env.NODE_ENV === "development";
  if (isDev && cfg && _isParseable(cfg.output)) {
    try {
      cfg.output.parse(result);
    } catch (e) {
      throw Object.assign(new Error("Invalid handler output"), {
        status: 500,
        code: "INTERNAL",
        details: { issues: _zodIssues(e) },
      });
    }
  }

  return result;
}

// _zsFetch(request, env, ctx) — WinterCG-symmetric HTTP handler.
//
// In the common case the kernel routes /_zs/v1/<id> directly through
// \`default.rpc\` and never invokes us. We're called here for two paths:
//   1. Non-/_zs/v1/* requests → forward to the user's own default.fetch
//      (when present), or 404.
//   2. RPC requests where the kernel fast path returned FallThrough
//      (the procedure resolved to an AsyncIterator) → re-dispatch
//      through _zsRpcAndRespond, which encodes the iterator as SSE.
async function _zsFetch(request, env, ctx) {
  const url = new URL(request.url);

  if (url.pathname.startsWith("/_zs/v1/")) {
    // Path-slice only — the wireId is byte-identical between the
    // kernel's fast path and our slow path. No percent-decoding, no
    // method override (the kernel restricts /_zs/v1/ to POST/GET, and
    // anything else 405s here).
    const id = url.pathname.slice("/_zs/v1/".length);
    if (!id) return _zsErrResponse(400, "INVALID_ARGUMENT", "missing wireId");

    let input = undefined;
    if (request.method === "GET") {
      const param = url.searchParams.get("input");
      if (param) {
        try {
          const b64 = param.replace(/-/g, "+").replace(/_/g, "/");
          const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
          const e = JSON.parse(atob(padded));
          input = e && typeof e === "object" && "json" in e ? e.json : e;
        } catch (e) {
          return _zsErrResponse(400, "INVALID_ARGUMENT", \`invalid base64url input: \${e?.message ?? e}\`);
        }
      }
    } else if (request.method === "POST") {
      const text = await request.text();
      if (text) {
        try {
          const e = JSON.parse(text);
          input = e && typeof e === "object" && "json" in e ? e.json : e;
        } catch (e) {
          return _zsErrResponse(400, "INVALID_ARGUMENT", \`invalid JSON body: \${e?.message ?? e}\`);
        }
      }
    } else {
      return _zsErrResponse(405, "FAILED_PRECONDITION", \`method \${request.method} not allowed on /_zs/v1/\`);
    }

    return await _zsRpcAndRespond(id, input, ctx);
  }

  if (_userFetch) return _userFetch.call(_userDefault, request, env, ctx);
  return new Response("Not Found", { status: 404 });
}

async function _zsRpcAndRespond(name, input, ctx) {
  try {
    const result = await _zsRpc(name, input, ctx);

    if (_isAsyncIterator(result)) {
      // Vercel AI-SDK Data Stream Protocol — line-prefixed framing:
      //   0:"text"\\n          string yields
      //   2:[<json>]\\n        object yields
      //   e:{...}\\n           structured error envelope (zeroship ext)
      //   d:{}\\n              done
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

    // Wrap in superjson \`{ json }\` envelope on the spec wire.
    return new Response(
      JSON.stringify({ json: result === undefined ? null : result }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  } catch (err) {
    const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600)
      ? err.status : 500;
    const body = {
      message: err?.message ?? String(err),
      name: err?.name ?? "Error",
    };
    if (err && typeof err.code === "string") body.code = err.code;
    if (err && err.details !== undefined) body.details = err.details;
    if (err && typeof err.retryable === "boolean") body.retryable = err.retryable;
    return new Response(
      JSON.stringify(body),
      { status, headers: { "content-type": "application/json" } },
    );
  }
}

export default { fetch: _zsFetch, rpc: _zsRpc };
`;
}

// ── Server-Binding Synthetic Entry ────────────────────────────────────────
//
// When the plugin has run the reference-graph walk it can hand the
// generator a `ServerBinding` map keyed by `<sourceFile>::<exportName>`.
// We emit one ESM import per target file (deduplicated) and a static
// `_procedures` literal keyed by wireId. This matches the shape in
// `docs/proposals/rpc-v2.md` §5.

function buildPhase2Entry(
  userImport: string,
  bindings: Map<string, ServerBinding>,
): string {
  // Group bindings by source file so we emit ONE namespace import per
  // target. The synthetic entry then references `<ns>.<exportName>`.
  // A file with both eager AND lazy exports lands in BOTH columns: the
  // namespace import for the eager ones, the dynamic-import wrapper for
  // the lazy ones. V8 de-dupes the module — the dynamic import resolves
  // to the same namespace the static import already evaluated (Wave
  // #187 host callback cache). The lazy wrapper is then a Map lookup
  // on the second call.
  const byFile = new Map<string, ServerBinding[]>();
  for (const b of bindings.values()) {
    const arr = byFile.get(b.sourceFile);
    if (arr) arr.push(b);
    else byFile.set(b.sourceFile, [b]);
  }

  // Files needing a static namespace import — those with at least one
  // EAGER binding. Lazy-only files get no static import (their dynamic
  // import is the only entry point).
  const eagerFiles = [...byFile.keys()]
    .filter((f) => byFile.get(f)!.some((b) => !b.lazy))
    .sort();
  const aliasOf = new Map<string, string>();
  eagerFiles.forEach((file, idx) => aliasOf.set(file, `_user_TARGET_${idx}_`));

  const importLines = eagerFiles
    .map((file) => `import * as ${aliasOf.get(file)} from ${JSON.stringify(file)};`)
    .join("\n");

  // Procedure entries — eager ones reference the static namespace
  // alias, lazy ones emit an arrow that does the dynamic import.
  // Ordering: iterate files lex, then bindings in their original order.
  const tableEntries: string[] = [];
  const allFiles = [...byFile.keys()].sort();
  for (const file of allFiles) {
    for (const b of byFile.get(file)!) {
      if (b.lazy) {
        // Wave #188 — defers module evaluation to first call. The
        // V8 dynamic-import host callback (Wave #187) caches the
        // namespace, so the second call is a Map lookup.
        tableEntries.push(
          `  ${JSON.stringify(b.wireId)}: async (input, ctx) => ` +
            `(await import(${JSON.stringify(file)})).${b.exportName}(input, ctx),`,
        );
      } else {
        tableEntries.push(
          `  ${JSON.stringify(b.wireId)}: ${aliasOf.get(file)}.${b.exportName},`,
        );
      }
    }
  }

  return `// virtual:zeroship/_server-entry — auto-generated synthetic entry
//
// The dispatch table is statically derived from the reference-graph
// walk. Each entry maps a wireId to a per-target namespace member.
// User code never reaches this module; the resolveId hook keeps it
// behind a \\0-prefix.

import * as _zsUser from ${userImport};
${importLines}

// TODO(rpc-v2): swap to \`__dispatchRpc\` from \`@zeroship/server/runtime\`
// once the upstream stub ships. Until then, the inline _zsRpc helper
// below preserves the current dispatch behavior (input parse, output
// dev-validation, stream tagging).

const _procedures = {
${tableEntries.join("\n")}
};

const _userDefault = (_zsUser && _zsUser.default && typeof _zsUser.default === "object")
  ? _zsUser.default : null;
const _userFetch = _userDefault && typeof _userDefault.fetch === "function"
  ? _userDefault.fetch : null;

function _zsErrResponse(status, code, message, details) {
  const body = { message, name: "Error", code };
  if (details !== undefined) body.details = details;
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function _isAsyncIterator(x) {
  return (
    x != null &&
    typeof x === "object" &&
    typeof x[Symbol.asyncIterator] === "function" &&
    typeof x.next === "function"
  );
}

function _isParseable(s) {
  return s != null && typeof s === "object" && typeof s.parse === "function";
}

function _zodIssues(err) {
  if (err && Array.isArray(err.issues)) return err.issues;
  if (err && Array.isArray(err.errors)) return err.errors;
  return [];
}

function _isZodStringSchema(s) {
  if (!s || typeof s !== "object") return false;
  const def = s._def || s.def;
  if (!def) return false;
  if (def.typeName === "ZodString") return true;
  if (def.type === "string") return true;
  return false;
}

// Phase-2 (static dispatch) _zsRpc. Mirrors the runtime path above —
// including B3 capability marker (__zsEnterKind / __zsExitKind around
// the user handler) AND the T1 follow-up auto-tx wrapper
// (__zsBeginAutoTx / __zsEndAutoTx) for query/mutation kinds.
function _zsRpc(name, input, ctx) {
  const fn = _procedures[name];
  if (typeof fn !== "function") {
    throw Object.assign(new Error("Method not found: " + name), {
      status: 404,
      code: "NOT_FOUND",
    });
  }

  let validated = input;
  const cfg = fn.config;
  if (cfg && _isParseable(cfg.input)) {
    try {
      validated = cfg.input.parse(input);
    } catch (e) {
      throw Object.assign(new Error("Invalid input"), {
        status: 400,
        code: "INVALID_ARGUMENT",
        details: { issues: _zodIssues(e) },
      });
    }
  }

  const kind = (cfg && typeof cfg.kind === "string" && cfg.kind) ||
               (typeof fn.__zsKind === "string" && fn.__zsKind) ||
               undefined;
  const ek = (typeof globalThis !== "undefined") ? globalThis.__zsEnterKind : undefined;
  const xk = (typeof globalThis !== "undefined") ? globalThis.__zsExitKind : undefined;
  const tok = (kind && typeof ek === "function") ? ek(kind) : -1;

  const bt = (typeof globalThis !== "undefined") ? globalThis.__zsBeginAutoTx : undefined;
  const et = (typeof globalThis !== "undefined") ? globalThis.__zsEndAutoTx : undefined;
  const wantsAutoTx = (kind === "query" || kind === "mutation") &&
                      typeof bt === "function" && typeof et === "function";
  if (wantsAutoTx) {
    return _zsRpcWithAutoTx(fn, validated, ctx, cfg, kind, tok, xk, bt, et);
  }

  try {
    const out = fn(validated, ctx);
    if (out && typeof out.then === "function") {
      return out.then(
        (v) => { if (tok >= 0 && typeof xk === "function") xk(tok); return _zsRpcPost(v, cfg); },
        (e) => { if (tok >= 0 && typeof xk === "function") xk(tok); throw e; },
      );
    }
    const post = _zsRpcPost(out, cfg);
    if (tok >= 0 && typeof xk === "function") xk(tok);
    return post;
  } catch (e) {
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw e;
  }
}

async function _zsRpcWithAutoTx(fn, input, ctx, cfg, _kind, tok, xk, bt, et) {
  let token = 0;
  try {
    token = await bt(_kind);
  } catch (beginErr) {
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw beginErr;
  }
  let out;
  try {
    out = fn(input, ctx);
    if (out && typeof out.then === "function") out = await out;
  } catch (handlerErr) {
    try { await et(token, false); } catch (_rb) { /* swallowed */ }
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw handlerErr;
  }
  try {
    await et(token, true);
  } catch (commitErr) {
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw commitErr;
  }
  const post = _zsRpcPost(out, cfg);
  if (tok >= 0 && typeof xk === "function") xk(tok);
  return post;
}

function _zsRpcPost(result, cfg) {
  if (_isAsyncIterator(result)) {
    if (cfg && _isZodStringSchema(cfg.output)) {
      try { result.__zsOutputIsString = true; } catch (_e) {}
    }
    return result;
  }
  const isDev =
    typeof process !== "undefined" &&
    process &&
    process.env &&
    process.env.NODE_ENV === "development";
  if (isDev && cfg && _isParseable(cfg.output)) {
    try {
      cfg.output.parse(result);
    } catch (e) {
      throw Object.assign(new Error("Invalid handler output"), {
        status: 500,
        code: "INTERNAL",
        details: { issues: _zodIssues(e) },
      });
    }
  }
  return result;
}

async function _zsFetch(request, env, ctx) {
  const url = new URL(request.url);

  if (url.pathname.startsWith("/_zs/v1/")) {
    const id = url.pathname.slice("/_zs/v1/".length);
    if (!id) return _zsErrResponse(400, "INVALID_ARGUMENT", "missing wireId");

    let input = undefined;
    if (request.method === "GET") {
      const param = url.searchParams.get("input");
      if (param) {
        try {
          const b64 = param.replace(/-/g, "+").replace(/_/g, "/");
          const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
          const e = JSON.parse(atob(padded));
          input = e && typeof e === "object" && "json" in e ? e.json : e;
        } catch (e) {
          return _zsErrResponse(400, "INVALID_ARGUMENT", \`invalid base64url input: \${e?.message ?? e}\`);
        }
      }
    } else if (request.method === "POST") {
      const text = await request.text();
      if (text) {
        try {
          const e = JSON.parse(text);
          input = e && typeof e === "object" && "json" in e ? e.json : e;
        } catch (e) {
          return _zsErrResponse(400, "INVALID_ARGUMENT", \`invalid JSON body: \${e?.message ?? e}\`);
        }
      }
    } else {
      return _zsErrResponse(405, "FAILED_PRECONDITION", \`method \${request.method} not allowed on /_zs/v1/\`);
    }

    return await _zsRpcAndRespond(id, input, ctx);
  }

  if (_userFetch) return _userFetch.call(_userDefault, request, env, ctx);
  return new Response("Not Found", { status: 404 });
}

async function _zsRpcAndRespond(name, input, ctx) {
  try {
    const result = await _zsRpc(name, input, ctx);

    if (_isAsyncIterator(result)) {
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
      JSON.stringify({ json: result === undefined ? null : result }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  } catch (err) {
    const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600)
      ? err.status : 500;
    const body = {
      message: err?.message ?? String(err),
      name: err?.name ?? "Error",
    };
    if (err && typeof err.code === "string") body.code = err.code;
    if (err && err.details !== undefined) body.details = err.details;
    if (err && typeof err.retryable === "boolean") body.retryable = err.retryable;
    return new Response(
      JSON.stringify(body),
      { status, headers: { "content-type": "application/json" } },
    );
  }
}

export default {
  rpc:   (name, input, ctx) => _zsRpc(name, input, ctx),
  fetch: (request, env, ctx) => _userFetch
    ? (new URL(request.url).pathname.startsWith("/_zs/v1/")
        ? _zsFetch(request, env, ctx)
        : _userFetch.call(_userDefault, request, env, ctx))
    : _zsFetch(request, env, ctx),
};
`;
}

// ── Vite plugin ────────────────────────────────────────────────────────────

/**
 * Vite plugin that resolves and loads the synthetic SSR entry.
 *
 * `enforce: "pre"` — runs before user-installed plugins so the virtual id
 * never leaks to the file resolver. The plugin owns this specifier in
 * full: any `virtual:zeroship/_server-entry` import in the graph routes
 * through `load` here.
 *
 * The entry's body is build-time-static. The dispatch table is
 * populated at module-init time by iterating the user module's
 * namespace exports — no shared mutable state, no order-dependence on
 * the transform pass, no virtual registry module.
 *
 * @param opts.root          Project root (unused; reserved for future
 *                           build-time procedure discovery if perf
 *                           argues for it).
 * @param opts.userEntryRel  Path the synthetic entry should import from.
 * @param opts.state         TransformState (unused; kept so call sites
 *                           can pass shared state without a refactor).
 */
export function rpcRegistryPlugin(opts: {
  root?: string;
  userEntryRel: string;
  state?: TransformState;
  /** Pre-computed server bindings. When provided, the generated entry
   *  uses static per-target imports plus a wireId-keyed `_procedures`
   *  literal. Recomputed by the caller (for example `buildPlugin`)
   *  after the reference-graph walk. */
  getBindings?: () => Map<string, ServerBinding> | undefined;
}): Plugin {
  return {
    name: "zeroship:server-entry",
    enforce: "pre",
    resolveId(id: string) {
      if (id === SERVER_ENTRY_VIRTUAL_ID) return SERVER_ENTRY_RESOLVED_ID;
      return null;
    },
    load(id: string) {
      if (id !== SERVER_ENTRY_RESOLVED_ID) return null;
      const bindings = opts.getBindings?.();
      return buildServerEntrySource({
        userEntryRel: opts.userEntryRel,
        bindings,
      });
    },
  };
}
