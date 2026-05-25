// Embedded RPC dispatcher (`__zsDispatch`).
//
// Compiled to `dist/dispatcher.js` and `include_str!`d by the runtime
// crate's `crates/runtime/src/core/init.rs`, spliced into the bootstrap
// module so the IIFE evaluates BEFORE `runtime-entry.js`'s top-level
// await and BEFORE the kernel resolves `default.fetch` / `default.rpc`
// off the user namespace. Dispatcher install must survive a schema-load
// failure so the worker can still surface the error via the RPC wire —
// hence: dispatcher first, schema install second.
//
// The IIFE pattern ensures idempotent install: if the bootstrap script
// is evaluated more than once (isolate refresh), the second pass keeps
// the live `__zsDispatch` rather than overwriting it. The function-
// shape `default.rpc` path documented in `docs/reference/zs-standard.md`
// bypasses this dispatcher entirely; the bootstrap calls the function
// directly.
//
// SOURCE OF TRUTH: this file is the canonical dispatcher. Dev mode
// (Vite plugin's `dev-bootstrap`) installs it via `dev-entry.ts` which
// imports this module for its side effect; production splices it into
// the runtime's bootstrap module via `include_str!`. Single
// implementation; no drift between dev and prod.

// Build emits this file with `export {};` to mark it as a module. The
// post-build step in `scripts/post-build.mjs` strips that line (and
// source-map comments) so the file content is pure top-level JS,
// safe to splice into the runtime's bootstrap module.
export {};

declare const globalThis: {
  __zsDispatch?: unknown;
  __zsEnterKind?: (kind: string) => number;
  __zsExitKind?: (token: number) => void;
  __zsValidateOutput?: boolean;
  [key: string]: unknown;
};

(function installZsDispatch(globalScope: typeof globalThis) {
  if (typeof globalScope.__zsDispatch === "function") return; // idempotent

  function isAsyncIterator(x: unknown): boolean {
    return x != null && typeof x === "object"
      && typeof (x as { [k: symbol]: unknown })[Symbol.asyncIterator] === "function"
      && typeof (x as { next?: unknown }).next === "function";
  }

  function isParseable(s: unknown): s is { parse: (input: unknown) => unknown } {
    return s != null && typeof s === "object" && typeof (s as { parse?: unknown }).parse === "function";
  }

  function zodIssues(err: unknown): unknown[] {
    const e = err as { issues?: unknown; errors?: unknown } | null;
    if (e && Array.isArray(e.issues)) return e.issues;
    if (e && Array.isArray(e.errors)) return e.errors;
    return [];
  }

  function isZodStringSchema(s: unknown): boolean {
    if (!s || typeof s !== "object") return false;
    const obj = s as {
      _def?: { typeName?: string; type?: string };
      def?: { typeName?: string; type?: string };
    };
    const def = obj._def || obj.def;
    if (!def) return false;
    if (def.typeName === "ZodString") return true;
    if (def.type === "string") return true;
    return false;
  }

  function mkErr(message: string, status: number, code: string, details?: unknown): Error {
    const e = new Error(message) as Error & { status?: number; code?: string; details?: unknown };
    e.status = status;
    e.code = code;
    if (details !== undefined) e.details = details;
    return e;
  }

  globalScope.__zsDispatch = async function dispatch(
    rpcDict: Record<string, unknown> | null | undefined,
    name: string,
    input: unknown,
    ctx: unknown,
  ): Promise<unknown> {
    if (rpcDict == null || typeof rpcDict !== "object") {
      throw mkErr("No RPC dispatch table installed", 500, "INTERNAL");
    }
    const fn = (rpcDict as Record<string, unknown>)[name] as
      | ((input: unknown, ctx: unknown) => unknown)
      | undefined;
    if (typeof fn !== "function") {
      throw mkErr("Method not found: " + name, 404, "NOT_FOUND");
    }

    const cfg = (fn as { config?: { input?: unknown; output?: unknown; kind?: string } }).config;

    // 1. Input validation.
    let validated = input;
    if (cfg && isParseable(cfg.input)) {
      try {
        validated = cfg.input.parse(input);
      } catch (e) {
        throw mkErr("Invalid input", 400, "INVALID_ARGUMENT", { issues: zodIssues(e) });
      }
    }

    // 2. Capability frame. Falls back to no-op when natives aren't
    //    installed (legacy embeddings / tests without DbPlugin).
    const kind = (cfg && typeof cfg.kind === "string") ? cfg.kind : undefined;
    const ek = globalScope.__zsEnterKind;
    const xk = globalScope.__zsExitKind;
    const tok = (kind && typeof ek === "function") ? ek(kind) : -1;

    try {
      const result = await fn(validated, ctx);

      // 3. AsyncIterator stream framing tag. The encoder reads
      //    __zsOutputIsString to decide between AI-SDK `0:` (text) and
      //    `2:` (object) lanes.
      if (isAsyncIterator(result)) {
        if (cfg && isZodStringSchema(cfg.output)) {
          try { (result as { __zsOutputIsString?: boolean }).__zsOutputIsString = true; } catch (_e) { /* frozen */ }
        }
        return result;
      }

      // 4. Dev-only output validation. Gated on the future runtime-
      //    controlled `__zsValidateOutput` flag — opt-in, default-off.
      if (cfg && isParseable(cfg.output) && globalScope.__zsValidateOutput) {
        try {
          cfg.output.parse(result);
        } catch (e) {
          throw mkErr("Invalid handler output", 500, "INTERNAL", { issues: zodIssues(e) });
        }
      }

      return result;
    } finally {
      if (tok >= 0 && typeof xk === "function") xk(tok);
    }
  };
})(globalThis as never);
