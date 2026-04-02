// Shared benchmark scenarios — loaded by both V8 server and Node.js server.
// Each method is an exported function: function(params...) -> result | Promise<result>

export function ping() { return "pong"; }

export function fib(n) {
    function fib(n) { return n <= 1 ? n : fib(n - 1) + fib(n - 2); }
    return fib(n);
}

export function timeout0() {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve("done"); }, 0);
    });
}

export function promiseChain() {
    return Promise.resolve(1)
        .then(function(v) { return v + 10; })
        .then(function(v) { return v * 2; });
}

export function promiseChainTimeout() {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve(1); }, 100);
    }).then(function(v) { return v + 10; })
      .then(function(v) { return v * 2; });
}

export async function fetchExternal(url) {
    var resp = await fetch(url);
    var data = await resp.json();
    return { status: resp.status, url: resp.url };
}
