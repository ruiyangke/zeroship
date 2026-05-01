// Installed on every isolate by setup_globals BEFORE user modules
// evaluate. The runtime owns Web APIs (fetch / streams / crypto / URL
// / WebSocket / console) and per-isolate state (process / process.env)
// — see init.rs::setup_globals.
//
// Node compat (Buffer, node:os, node:path, …) is the build pipeline's
// job: `@zeroship/vite-plugin` wires unenv@2's `defineEnv()` so bare
// `Buffer.from(...)` references get rewritten by @rollup/plugin-inject
// into `import { Buffer } from "node:buffer"`, which alias-resolves to
// `unenv/node/buffer`. Result: bare and explicit reads land on one
// class, with no runtime stub to drift out of spec.
//
// What stays here is what doesn't fit the inject pattern cleanly:
//   - setImmediate / clearImmediate: Node-only timers. Map to
//     setTimeout(0) / clearTimeout. Three lines, not worth a build pass.

(function () {
  const g = globalThis;

  if (!g.setImmediate) {
    g.setImmediate = (fn, ...args) => setTimeout(() => fn(...args), 0);
    g.clearImmediate = (id) => clearTimeout(id);
  }
})();
