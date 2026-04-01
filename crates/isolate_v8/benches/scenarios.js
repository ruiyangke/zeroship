// Shared benchmark scenarios — loaded by both V8 server and Node.js server.
// Each method is a JSON-RPC handler: function(params...) → result | Promise<result>

var __rpc = {
    ping: function() { return "pong"; },

    fib: function(n) {
        function fib(n) { return n <= 1 ? n : fib(n - 1) + fib(n - 2); }
        return fib(n);
    },

    timeout0: function() {
        return new Promise(function(resolve) {
            setTimeout(function() { resolve("done"); }, 0);
        });
    },

    promiseChain: function() {
        return Promise.resolve(1)
            .then(function(v) { return v + 10; })
            .then(function(v) { return v * 2; });
    },

    promiseChainTimeout: function() {
        return new Promise(function(resolve) {
            setTimeout(function() { resolve(1); }, 100);
        }).then(function(v) { return v + 10; })
          .then(function(v) { return v * 2; });
    },

    fetchEcho: async function(url) {
        var resp = await fetch(url, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ ts: Date.now() }),
        });
        return await resp.json();
    },

    fetchExternal: async function(url) {
        var resp = await fetch(url);
        var data = await resp.json();
        return { status: resp.status, url: resp.url };
    },
};

// Node.js: export for require()
if (typeof module !== "undefined" && module.exports) {
    module.exports = __rpc;
}
