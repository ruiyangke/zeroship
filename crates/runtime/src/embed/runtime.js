// Appbase runtime - injected before user code
// Bridges JS calls to Rust ops via serde_v8 (no manual JSON serialization)

const { core } = Deno;

// Console
globalThis.console = {
  log: (...args) => {
    core.print(args.map(a => typeof a === 'string' ? a : JSON.stringify(a)).join(' ') + '\n', false);
  },
  error: (...args) => {
    core.print(args.map(a => typeof a === 'string' ? a : JSON.stringify(a)).join(' ') + '\n', true);
  },
};

// DB primitive - ops use serde_v8, objects pass directly without JSON round-trip
globalThis.db = {
  collection(name) {
    core.ops.op_db_ensure_table(name);
    return {
      insert: (doc) => core.ops.op_db_insert(name, doc),
      find: (filter) => core.ops.op_db_find(name, filter || {}),
      update: (id, updates) => core.ops.op_db_update(name, id, updates),
      delete: (id) => core.ops.op_db_delete(name, id),
    };
  },
};

// RPC dispatcher - user code registers functions here
globalThis.__rpc = {};

// Pre-compiled RPC dispatch function - called from Rust via op, NOT via execute_script
// This avoids V8 re-parsing/compiling JS on every request
globalThis.__handleRpc = async function(requestJson) {
  const request = JSON.parse(requestJson);
  if (Array.isArray(request)) {
    return JSON.stringify(await Promise.all(request.map(__dispatch)));
  }
  return JSON.stringify(await __dispatch(request));
};

async function __dispatch(req) {
  try {
    const fn_ = globalThis.__rpc[req.method];
    if (!fn_) {
      return { jsonrpc: '2.0', error: { code: -32601, message: 'Method not found: ' + req.method }, id: req.id };
    }
    const result = await fn_(...(req.params || []));
    return { jsonrpc: '2.0', result, id: req.id };
  } catch (e) {
    return { jsonrpc: '2.0', error: { code: -32000, message: e.message || String(e) }, id: req.id };
  }
}
