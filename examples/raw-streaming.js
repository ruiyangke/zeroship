// Richer raw-JS RPC demo — exercises the ZS-standard dispatcher beyond
// the trivial sync handlers in `raw-rpc.js`. No Vite, no `@zeroship/*`
// imports, no tooling — just a JS module exporting the documented
// `default = { rpc }` shape.
//
// Demonstrates four surfaces of the Stage 5a dispatcher:
//   1. `config.kind = "query"`          → capability frame (no-op without
//                                          plugin-db; auto-tx wraps when
//                                          plugin-db is loaded).
//   2. `config.input.parse(v)`          → pre-handler validation. Returns
//                                          400 INVALID_ARGUMENT on throw,
//                                          surfacing `issues[]` on the
//                                          wire — matches Zod's shape.
//   3. async generator return           → streaming procedure. Over POST
//                                          /_zs/v1/<id>, raw-JS deploys
//                                          fall through to default.fetch
//                                          (which this demo doesn't
//                                          expose), so the canonical wire
//                                          is WebSocket subscription:
//                                          `wss://host/_zs/v1/<id>` with
//                                          a `{"t":"hello",input}` frame.
//                                          The runtime's
//                                          `dispatchSubscription` invokes
//                                          the dict-shape handler via
//                                          `__zsDispatch` and pumps each
//                                          yield as a `{"t":"data",value}`
//                                          frame.
//   4. plain fn that calls `fetch(...)` → action-like. No kind set; the
//                                          dispatcher applies no capability
//                                          frame and no auto-tx.
//
// Run:
//   target/release/zeroship serve examples/raw-streaming.js --port 3000
//
// Smoke (unary procedures over POST):
//   curl -X POST http://localhost:3000/_zs/v1/search \
//        -H 'content-type: application/json' \
//        -d '{"json":{"q":"hello","limit":3}}'
//   → {"json":{"hits":[...],"meta":{"q":"hello","limit":3}}}
//
//   curl -X POST http://localhost:3000/_zs/v1/search \
//        -H 'content-type: application/json' \
//        -d '{"json":{"limit":3}}'
//   → 400 {"message":"Invalid input",...,"details":{"issues":[...]}}
//
//   curl -X POST http://localhost:3000/_zs/v1/echoHeaders \
//        -H 'content-type: application/json' \
//        -d '{"json":"https://www.google.com/"}'
//   → {"json":{"status":200,"contentType":"text/html; charset=ISO-8859-1"}}
//
// Streaming smoke (WebSocket subscription — requires a WS client):
//   See `dispatchSubscription` in `crates/runtime/src/core/init.rs` for
//   the frame protocol. The tick handler below is wire-compatible.

// ── 1. `query` kind with an `input.parse()` validator (Zod-shape)
//
// `input.parse(v)` is invoked by the dispatcher BEFORE the handler runs.
// On throw, the dispatcher emits 400 INVALID_ARGUMENT with `issues[]`
// pulled from `err.issues || err.errors` (Zod's two surface shapes).
// Here we hand-roll the same shape so the demo runs without a Zod
// import — the dispatcher doesn't care which library produced the
// parser, only that the contract is `.parse(v) → validated | throw`.

function searchInputSchema() {
  return {
    parse(v) {
      const issues = [];
      if (!v || typeof v !== "object") {
        issues.push({ path: [], message: "expected object" });
      } else {
        if (typeof v.q !== "string" || v.q.length === 0) {
          issues.push({ path: ["q"], message: "expected non-empty string" });
        }
        if (v.limit !== undefined && (typeof v.limit !== "number" || v.limit < 1)) {
          issues.push({ path: ["limit"], message: "expected positive integer" });
        }
      }
      if (issues.length) {
        const err = new Error("validation failed");
        err.issues = issues;
        throw err;
      }
      return { q: v.q, limit: v.limit ?? 10 };
    },
  };
}

const search = (input) => {
  // A real handler would hit a search index. We return a deterministic
  // fixture so the demo is smoke-testable without external deps.
  const hits = [];
  for (let i = 0; i < input.limit; i++) {
    hits.push({ id: i, title: `${input.q} result ${i}` });
  }
  return { hits, meta: { q: input.q, limit: input.limit } };
};
search.config = {
  kind: "query",
  input: searchInputSchema(),
};

// ── 2. Streaming procedure (async generator → AsyncIterator)
//
// The dispatcher returns the AsyncIterator unchanged. Over POST, raw-JS
// deploys without a `default.fetch` fall back to the kernel's fallbackFetch
// (no native SSE encoder for unary RPC streams). Over a WS upgrade on
// `/_zs/v1/tick`, the kernel's `dispatchSubscription` pumps each yield as
// a `{"t":"data",value}` frame — that's the wire surface for raw streams.

const tick = async function* (input) {
  const count = (input && typeof input.count === "number") ? input.count : 5;
  for (let i = 0; i < count; i++) {
    yield { i, ts: Date.now() };
    // Yield to the event loop so back-pressure can apply between frames.
    await new Promise((r) => setTimeout(r, 10));
  }
};
tick.config = { kind: "subscription" };

// ── 3. Action — calls `fetch(...)`. No kind set.
//
// The dispatcher applies no capability frame (kind:undefined → tok=-1)
// and no auto-tx — so outbound fetch is unrestricted. Mirrors the
// `action` ergonomic in `@zeroship/server`.

const echoHeaders = async (input) => {
  const url = typeof input === "string" ? input : (input && input.url);
  if (typeof url !== "string") {
    const err = new Error("url (string) required");
    err.status = 400;
    err.code = "INVALID_ARGUMENT";
    throw err;
  }
  const resp = await fetch(url, { method: "HEAD" });
  return {
    status: resp.status,
    contentType: resp.headers.get("content-type") || null,
  };
};
// No echoHeaders.config — action-like surface (no kind marker).

// ── 4. Mutation — symmetric to query, also triggers auto-tx when
//      plugin-db is loaded. Without plugin-db, the capability frame is
//      a no-op and the handler runs unwrapped. Real apps would persist
//      via `env.db.*`; here we just echo the input back with a
//      synthesised id so the demo runs without any storage backend.
//
//      (Module-scope mutable state isn't shared across worker isolates
//      under `zeroship serve`, so this handler intentionally stays
//      stateless — keep mutable state in the database, not in JS.)

const recordNote = (input) => {
  if (!input || typeof input.text !== "string") {
    const err = new Error("text required");
    err.status = 400;
    err.code = "INVALID_ARGUMENT";
    throw err;
  }
  return { id: crypto.randomUUID(), text: input.text, recordedAt: Date.now() };
};
recordNote.config = { kind: "mutation" };

export default {
  rpc: { search, tick, echoHeaders, recordNote },
};
