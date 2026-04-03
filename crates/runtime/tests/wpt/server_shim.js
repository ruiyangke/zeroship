// WPT runner server shim — provides exported RPC methods for loading and running tests.
// Loaded as ES module in Isolate, with shims set up as globalThis properties.

var __wpt_init_error = null;

export function __load_harness(code) {
    try {
        (0, eval)(code);
        // Prevent ShellTestEnvironment's microtask from auto-completing.
        // It does: Promise.resolve().then(() => { all_loaded = true; callback(); })
        // We need tests to register BEFORE completion fires.
        // Hack: set explicit_done so the harness waits for our done() call.
        if (typeof setup === "function") {
            setup({ explicit_done: true });
        }
        return "ok";
    }
    catch(e) { return "error: " + e.message; }
}

export function __load_report(code) {
    try { (0, eval)(code); return "ok"; }
    catch(e) { return "error: " + e.message; }
}

export function __run_test(code) {
    try {
        (0, eval)(code);
        // Don't let the ShellTestEnvironment's microtask auto-complete
        // until we explicitly call done(). The microtask sets all_loaded=true
        // which triggers completion. We need all test() calls to finish first.
        return "ok, results=" + (globalThis.__wpt_results || []).length + " done=" + (!!globalThis.__wpt_done) + " test_count=" + globalThis.__wpt_test_count;
    } catch(e) {
        __wpt_init_error = e.message || String(e);
        return "error: " + e.message;
    }
}

export function __done() {
    if (typeof globalThis.done === "function") globalThis.done();
    return "ok, results=" + (globalThis.__wpt_results || []).length + " done=" + (!!globalThis.__wpt_done) + " test_count=" + globalThis.__wpt_test_count;
}

export function __wpt_run() { return JSON.stringify(globalThis.__wpt_results || []); }
export function __wpt_done() { return !!globalThis.__wpt_done; }
export function __wpt_error() { return __wpt_init_error || "none"; }
