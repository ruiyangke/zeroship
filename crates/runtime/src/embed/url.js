// URL and URLSearchParams — WHATWG spec-compliant via ada-url native parser.
// __urlParse and __urlCanParse are native Rust callbacks.

(function(globalThis) {
"use strict";

// =========================================================================
// URLSearchParams
// =========================================================================

function URLSearchParams(init) {
    this._params = [];

    if (typeof init === "string") {
        if (init.charAt(0) === "?") init = init.slice(1);
        var pairs = init.split("&");
        for (var i = 0; i < pairs.length; i++) {
            if (!pairs[i]) continue;
            var eq = pairs[i].indexOf("=");
            if (eq === -1) {
                this._params.push([decodeURIComponent(pairs[i]), ""]);
            } else {
                this._params.push([
                    decodeURIComponent(pairs[i].slice(0, eq)),
                    decodeURIComponent(pairs[i].slice(eq + 1).replace(/\+/g, " ")),
                ]);
            }
        }
    } else if (Array.isArray(init)) {
        for (var j = 0; j < init.length; j++) {
            this._params.push([String(init[j][0]), String(init[j][1])]);
        }
    } else if (init instanceof URLSearchParams) {
        this._params = init._params.map(function(p) { return p.slice(); });
    } else if (init && typeof init === "object") {
        var keys = Object.keys(init);
        for (var k = 0; k < keys.length; k++) {
            this._params.push([keys[k], String(init[keys[k]])]);
        }
    }
}

URLSearchParams.prototype.append = function(name, value) { this._params.push([String(name), String(value)]); };
URLSearchParams.prototype.delete = function(name) { this._params = this._params.filter(function(p) { return p[0] !== String(name); }); };
URLSearchParams.prototype.get = function(name) { name = String(name); for (var i = 0; i < this._params.length; i++) { if (this._params[i][0] === name) return this._params[i][1]; } return null; };
URLSearchParams.prototype.getAll = function(name) { name = String(name); return this._params.filter(function(p) { return p[0] === name; }).map(function(p) { return p[1]; }); };
URLSearchParams.prototype.has = function(name) { name = String(name); return this._params.some(function(p) { return p[0] === name; }); };
URLSearchParams.prototype.set = function(name, value) {
    name = String(name); value = String(value);
    var found = false;
    this._params = this._params.filter(function(p) {
        if (p[0] === name) { if (!found) { found = true; p[1] = value; return true; } return false; }
        return true;
    });
    if (!found) this._params.push([name, value]);
};
URLSearchParams.prototype.sort = function() { this._params.sort(function(a, b) { return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0; }); };
URLSearchParams.prototype.toString = function() {
    return this._params.map(function(p) {
        return encodeURIComponent(p[0]).replace(/%20/g, "+") + "=" + encodeURIComponent(p[1]).replace(/%20/g, "+");
    }).join("&");
};
URLSearchParams.prototype.forEach = function(cb, thisArg) { for (var i = 0; i < this._params.length; i++) cb.call(thisArg, this._params[i][1], this._params[i][0], this); };
URLSearchParams.prototype.entries = function() { var a = this._params, i = 0; return { next: function() { return i >= a.length ? { done: true } : { done: false, value: a[i++].slice() }; }, [Symbol.iterator]: function() { return this; } }; };
URLSearchParams.prototype.keys = function() { var a = this._params, i = 0; return { next: function() { return i >= a.length ? { done: true } : { done: false, value: a[i++][0] }; }, [Symbol.iterator]: function() { return this; } }; };
URLSearchParams.prototype.values = function() { var a = this._params, i = 0; return { next: function() { return i >= a.length ? { done: true } : { done: false, value: a[i++][1] }; }, [Symbol.iterator]: function() { return this; } }; };
URLSearchParams.prototype[Symbol.iterator] = function() { return this.entries(); };

// =========================================================================
// URL (backed by ada-url native parser)
// =========================================================================

function URL(input, base) {
    if (arguments.length === 0) throw new TypeError("Failed to construct 'URL': 1 argument required");

    var baseStr = (base !== undefined) ? String(base instanceof URL ? base.href : base) : undefined;
    var parsed = __urlParse(String(input), baseStr);
    if (parsed === null) throw new TypeError("Invalid URL: " + input);

    this._protocol = parsed.protocol;
    this._username = parsed.username;
    this._password = parsed.password;
    this._hostname = parsed.hostname;
    this._port = parsed.port;
    this._pathname = parsed.pathname;
    this._search = parsed.search;
    this._hash = parsed.hash;
    this._origin = parsed.origin;
    this._searchParams = new URLSearchParams(this._search);
}

Object.defineProperties(URL.prototype, {
    href: {
        get: function() {
            var auth = this._username ? (this._username + (this._password ? ":" + this._password : "") + "@") : "";
            var port = this._port ? ":" + this._port : "";
            return this._protocol + "//" + auth + this._hostname + port + this._pathname + this.search + this._hash;
        },
        set: function(v) { var u = new URL(v); Object.assign(this, { _protocol: u._protocol, _username: u._username, _password: u._password, _hostname: u._hostname, _port: u._port, _pathname: u._pathname, _search: u._search, _hash: u._hash, _origin: u._origin, _searchParams: new URLSearchParams(u._search) }); },
    },
    origin: { get: function() { return this._origin; } },
    protocol: { get: function() { return this._protocol; }, set: function(v) { this._protocol = v.endsWith(":") ? v : v + ":"; } },
    username: { get: function() { return this._username; }, set: function(v) { this._username = v; } },
    password: { get: function() { return this._password; }, set: function(v) { this._password = v; } },
    host: { get: function() { return this._hostname + (this._port ? ":" + this._port : ""); }, set: function(v) { var i = v.lastIndexOf(":"); if (i > -1) { this._hostname = v.slice(0, i); this._port = v.slice(i + 1); } else { this._hostname = v; this._port = ""; } } },
    hostname: { get: function() { return this._hostname; }, set: function(v) { this._hostname = v; } },
    port: { get: function() { return this._port; }, set: function(v) { this._port = v; } },
    pathname: { get: function() { return this._pathname; }, set: function(v) { this._pathname = v.charAt(0) === "/" ? v : "/" + v; } },
    search: { get: function() { var s = this._searchParams.toString(); return s ? "?" + s : ""; }, set: function(v) { this._search = v; this._searchParams = new URLSearchParams(v); } },
    searchParams: { get: function() { return this._searchParams; } },
    hash: { get: function() { return this._hash; }, set: function(v) { this._hash = v.charAt(0) === "#" ? v : (v ? "#" + v : ""); } },
});

URL.prototype.toString = function() { return this.href; };
URL.prototype.toJSON = function() { return this.href; };

URL.canParse = function(input, base) { return __urlCanParse(String(input), base !== undefined ? String(base) : undefined); };

// =========================================================================
// Export
// =========================================================================

globalThis.URL = URL;
globalThis.URLSearchParams = URLSearchParams;

})(globalThis);
