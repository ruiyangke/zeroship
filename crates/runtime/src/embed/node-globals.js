// Installed on every isolate by setup_globals BEFORE user modules
// evaluate. The runtime owns process / process.env / global /
// console / fetch / crypto / URL / streams / WebSocket — see
// init.rs::setup_globals. The two below are the ones it doesn't:
//
//   - Buffer: npm packages reach this as a bare global without
//             importing `node:buffer`. The proper Buffer comes from
//             unenv when code does `import { Buffer } from
//             "node:buffer"`. This stub catches the no-import path
//             with the most-used surface (.from / .alloc / .concat /
//             .isBuffer). Defined as a configurable getter so the
//             unenv import (which calls Object.defineProperty(globalThis,
//             "Buffer", ...)) can swap it out cleanly the first time.
//   - setImmediate / clearImmediate: Node-only timers. Map to
//             setTimeout(0) / clearTimeout. Many isomorphic libs
//             feature-detect setImmediate and prefer it over
//             setTimeout when present.

(function () {
  const g = globalThis;

  if (!g.Buffer) {
    Object.defineProperty(g, "Buffer", {
      configurable: true,
      get() {
        const stub = function (...args) { return Uint8Array.from(args[0] ?? []); };
        stub.from = (input, encoding) => {
          if (typeof input === "string") {
            if (encoding === "base64") {
              const bin = atob(input);
              const out = new Uint8Array(bin.length);
              for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
              return out;
            }
            return new TextEncoder().encode(input);
          }
          return new Uint8Array(input);
        };
        stub.alloc = (size, fill = 0) => new Uint8Array(size).fill(fill);
        stub.concat = (list) => {
          const len = list.reduce((s, b) => s + b.length, 0);
          const out = new Uint8Array(len);
          let off = 0;
          for (const b of list) { out.set(b, off); off += b.length; }
          return out;
        };
        stub.isBuffer = (x) => x instanceof Uint8Array;
        Object.defineProperty(g, "Buffer", { value: stub, configurable: true });
        return stub;
      },
    });
  }

  if (!g.setImmediate) {
    g.setImmediate = (fn, ...args) => setTimeout(() => fn(...args), 0);
    g.clearImmediate = (id) => clearTimeout(id);
  }
})();
