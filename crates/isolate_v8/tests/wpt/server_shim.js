// WPT runner server shim — provides __rpc methods for loading and running tests.
// Loaded as server_js in Isolate, with shims already in scope.

var __wpt_init_error = null;

var __rpc = {
    __load_harness: function(code) {
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
    },
    __load_report: function(code) {
        try { (0, eval)(code); return "ok"; }
        catch(e) { return "error: " + e.message; }
    },
    __run_test: function(code) {
        try {
            (0, eval)(code);
            // Don't let the ShellTestEnvironment's microtask auto-complete
            // until we explicitly call done(). The microtask sets all_loaded=true
            // which triggers completion. We need all test() calls to finish first.
            return "ok, results=" + (__wpt_results || []).length + " done=" + (!!__wpt_done) + " test_count=" + __wpt_test_count;
        } catch(e) {
            __wpt_init_error = e.message || String(e);
            return "error: " + e.message;
        }
    },
    __done: function() {
        if (typeof done === "function") done();
        return "ok, results=" + (__wpt_results || []).length + " done=" + (!!__wpt_done) + " test_count=" + __wpt_test_count;
    },
    __wpt_run: function() { return JSON.stringify(__wpt_results || []); },
    __wpt_done: function() { return !!__wpt_done; },
    __wpt_error: function() { return __wpt_init_error || "none"; }
};
