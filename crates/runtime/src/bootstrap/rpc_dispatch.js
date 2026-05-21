// Embedded RPC dispatcher (`__zsDispatch`).
//
// Inlined into BOOTSTRAP_JS (`crates/runtime/src/core/init.rs`) so it
// runs INSIDE the bootstrap module's top-level evaluation, BEFORE
// `db_init.js`'s top-level await on `installSchema` (a schema-loading
// error must not prevent the dispatcher from being installed) AND
// BEFORE the runtime resolves `default.rpc` off the user namespace.
// `db_init.js` reads schema from `user.default.schema` directly — no
// manifest-injected path.
//
// Invoked by init.rs's bootstrap when `user.default.rpc` is a plain
// object (dict-shape: `{ [wireId]: handler }`). The dispatcher owns:
//   - input validation via fn.config.input.parse()
//   - capability frame via __zsEnterKind / __zsExitKind
//   - auto-tx for query/mutation via __zsBeginAutoTx / __zsEndAutoTx
//   - AsyncIterator stream framing tag (__zsOutputIsString)
//   - dev-only output validation via fn.config.output.parse()
//
// Function-shape `default.rpc` is the documented advanced / back-compat
// path (`docs/reference/zs-standard.md`); the bootstrap calls it
// directly and this module never runs for that path.
//
// `__zsValidateOutput` is a runtime-controlled flag (dev only). Treat
// any cfg.output as opt-in via that global; default-off matches
// production behaviour.
//
// The IIFE pattern ensures idempotent install: if the bootstrap script
// is evaluated more than once (isolate refresh), the second pass keeps
// the live `__zsDispatch` rather than overwriting it.
(function installZsDispatch(globalScope) {
  if (typeof globalScope.__zsDispatch === "function") return; // idempotent

  function isAsyncIterator(x) {
    return x != null && typeof x === "object"
      && typeof x[Symbol.asyncIterator] === "function"
      && typeof x.next === "function";
  }

  function isParseable(s) {
    return s != null && typeof s === "object" && typeof s.parse === "function";
  }

  function zodIssues(err) {
    if (err && Array.isArray(err.issues)) return err.issues;
    if (err && Array.isArray(err.errors)) return err.errors;
    return [];
  }

  // Detect a Zod string schema (for AI-SDK `0:` text-part wire framing).
  // Mirrors `_isZodStringSchema` in the synthetic-entry helpers.
  function isZodStringSchema(s) {
    if (!s || typeof s !== "object") return false;
    const def = s._def || s.def;
    if (!def) return false;
    if (def.typeName === "ZodString") return true;
    if (def.type === "string") return true;
    return false;
  }

  function mkErr(message, status, code, details) {
    const e = new Error(message);
    e.status = status;
    e.code = code;
    if (details !== undefined) e.details = details;
    return e;
  }

  globalScope.__zsDispatch = async function dispatch(rpcDict, name, input, ctx) {
    if (rpcDict == null || typeof rpcDict !== "object") {
      throw mkErr("No RPC dispatch table installed", 500, "INTERNAL");
    }
    const fn = rpcDict[name];
    if (typeof fn !== "function") {
      throw mkErr("Method not found: " + name, 404, "NOT_FOUND");
    }

    const cfg = fn.config;

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

    // 3. Auto-tx for query/mutation when plugin-db's natives are present.
    const bt = globalScope.__zsBeginAutoTx;
    const et = globalScope.__zsEndAutoTx;
    const wantsAutoTx = (kind === "query" || kind === "mutation")
                       && typeof bt === "function" && typeof et === "function";

    try {
      let result;
      if (wantsAutoTx) {
        const isolation = (cfg && typeof cfg.isolation === "string") ? cfg.isolation : "";
        let token = 0;
        try {
          token = await bt(kind, isolation);
        } catch (beginErr) {
          throw beginErr;
        }
        try {
          result = await fn(validated, ctx);
        } catch (handlerErr) {
          try { await et(token, false); } catch (_rb) { /* swallow rollback errs */ }
          throw handlerErr;
        }
        // Commit. Commit failure becomes the caller-visible error
        // (data integrity wins, mirroring _zsRpcWithAutoTx).
        await et(token, true);
      } else {
        result = await fn(validated, ctx);
      }

      // 4. AsyncIterator stream framing tag. The encoder reads
      //    __zsOutputIsString to decide between AI-SDK `0:` (text) and
      //    `2:` (object) lanes.
      if (isAsyncIterator(result)) {
        if (cfg && isZodStringSchema(cfg.output)) {
          try { result.__zsOutputIsString = true; } catch (_e) { /* frozen */ }
        }
        return result;
      }

      // 5. Dev-only output validation. Gated on the future runtime-
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
})(globalThis);
