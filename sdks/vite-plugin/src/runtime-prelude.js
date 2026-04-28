// Prepended to every server bundle so deepagents + langchain (and
// any other npm package that does `globalThis.process.env.X`) finds
// the symbols it expects without us having to teach the bundler to
// rewrite every reference.
//
// Anything heavier (real EventEmitter, Buffer, etc.) is provided
// via the unenv polyfills — those are imported by the bundled code
// when it `import`s a `node:*` specifier. The shims below exist for
// code that reads `process.env.X` etc. WITHOUT importing
// `node:process` first (very common in defensive feature-detection).

(function installNodeGlobals() {
  if (typeof globalThis === "undefined") return;
  const g = globalThis;

  // process
  if (!g.process) {
    g.process = {
      env: {},
      argv: [],
      argv0: "node",
      pid: 1,
      ppid: 0,
      title: "zeroship",
      versions: { node: "22.0.0", v8: "12.0.0", openssl: "3.0.0" },
      version: "v22.0.0",
      platform: "linux",
      arch: "x64",
      release: { name: "node" },
      cwd() { return "/"; },
      chdir() { /* no-op */ },
      exit() { throw new Error("process.exit() called in zeroship runtime"); },
      nextTick(fn, ...args) { queueMicrotask(() => fn(...args)); },
      hrtime: Object.assign(
        function hrtime(prev) {
          const ms = (typeof performance !== "undefined" ? performance.now() : Date.now());
          const sec = Math.floor(ms / 1000);
          const nsec = Math.floor((ms - sec * 1000) * 1e6);
          if (prev) {
            const dsec = sec - prev[0];
            const dnsec = nsec - prev[1];
            return [dsec, dnsec];
          }
          return [sec, nsec];
        },
        { bigint() { return BigInt(Math.floor((typeof performance !== "undefined" ? performance.now() : Date.now()) * 1e6)); } },
      ),
      stdout: { write(s) { console.log(typeof s === "string" ? s.replace(/\n$/, "") : s); }, isTTY: false },
      stderr: { write(s) { console.error(typeof s === "string" ? s.replace(/\n$/, "") : s); }, isTTY: false },
      stdin: { read() { return null; }, isTTY: false },
      emitWarning(msg) { console.warn("warning:", msg); },
      on() { /* no-op */ },
      off() { /* no-op */ },
      once() { /* no-op */ },
      removeListener() { /* no-op */ },
      removeAllListeners() { /* no-op */ },
      listeners() { return []; },
      addListener() { /* no-op */ },
      setMaxListeners() { /* no-op */ },
      getMaxListeners() { return 10; },
      eventNames() { return []; },
    };
    // Hand-injected env vars from the zeroship runtime's `env` object —
    // user-set vars + secrets surface here for code that reads
    // process.env.X (langchain reads OPENAI_API_KEY, ANTHROPIC_API_KEY,
    // etc. this way).
    if (g.env && typeof g.env === "object") {
      for (const k of Object.keys(g.env)) {
        try { g.process.env[k] = g.env[k]; } catch { /* readonly */ }
      }
    }
  }

  // Common Node aliases / globals
  if (!g.global) g.global = g;
  if (!g.Buffer && g.process) {
    // Lazy: only define when something dereferences it. The unenv
    // `node:buffer` polyfill provides the real impl when imported.
    Object.defineProperty(g, "Buffer", {
      configurable: true,
      get() {
        try {
          // eslint-disable-next-line @typescript-eslint/no-var-requires
          const m = (g.__zsBufferModule__ = g.__zsBufferModule__ || null);
          if (m) return m.Buffer;
        } catch {}
        // Fallback stub — most code paths never get here because
        // the unenv polyfill resolves first via the `node:buffer`
        // import. This guard exists for direct `Buffer.from(...)`
        // calls that bypass the import.
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

  // setImmediate — Node-only timer. Maps onto queueMicrotask for
  // best fidelity (microtask queue runs before next macrotask).
  if (!g.setImmediate) {
    g.setImmediate = (fn, ...args) => {
      const id = setTimeout(() => fn(...args), 0);
      return id;
    };
    g.clearImmediate = (id) => clearTimeout(id);
  }
})();
