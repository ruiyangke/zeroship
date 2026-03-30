// Appbase runtime - injected before user code
// Bridges JS calls to Rust ops

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

// DB primitive - collection-based API bridging to Rust SQLite ops
globalThis.db = {
  collection(name) {
    core.ops.op_db_ensure_table(name);

    return {
      async insert(doc) {
        const result = core.ops.op_db_insert(name, JSON.stringify(doc));
        return JSON.parse(result);
      },

      async find(filter) {
        const result = core.ops.op_db_find(name, JSON.stringify(filter || {}));
        return JSON.parse(result);
      },

      async update(id, updates) {
        const result = core.ops.op_db_update(name, id, JSON.stringify(updates));
        return JSON.parse(result);
      },

      async delete(id) {
        core.ops.op_db_delete(name, id);
      },
    };
  },
};

// RPC dispatcher - user code registers functions here
globalThis.__rpc = {};

// JSON-RPC dispatch function (called from Rust)
globalThis.__dispatch = async function(req) {
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
};
