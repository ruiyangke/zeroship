//! Native `node:util`.
//!
//! Most of `util` is pure-JS data-shaping with no syscall surface, so we
//! compile a single JS string at evaluation time, run it to build a
//! namespace object, then mirror each property onto the synthetic
//! module's named exports (plus `default`). This keeps the
//! implementation close to the documented Node spec without per-export
//! Rust callbacks.
//!
//! ## Surface (Node 22)
//!
//! Covered: `format`, `inspect` (+ `colors` / `styles` stubs),
//! `promisify`, `callbackify`, `deprecate`, `types.*` predicates,
//! `isDeepStrictEqual`, `parseArgs` (long-form `--flag value` only),
//! and re-exports of `TextEncoder` / `TextDecoder` from `globalThis`.
//!
//! ## Deferred (Phase-2 candidates)
//!
//! - `inspect` quirks: custom `util.inspect.custom` symbol, Symbol
//!   keys, getters/setters, Date-as-ISO, RegExp source, BigInt suffix.
//! - `styleText` — depends on a terminal-color story we haven't picked.
//! - `parseArgs` short flags, multiples, `tokens`, strict + allow-positionals.
//! - `MIMEType`, `MIMEParams`, `getSystemErrorName`, `aborted`.
//! - `debuglog` / `debug` — needs a NODE_DEBUG env-var hook.

#![allow(unsafe_code)]

/// Mint a synthetic ESM record for `node:util`. Called from
/// `core::native_modules::resolve_native`.
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:util").unwrap();
    let names = export_names();
    let export_strings: Vec<v8::Local<v8::String>> = names
        .iter()
        .map(|n| v8::String::new(scope, n).unwrap())
        .collect();
    v8::Module::create_synthetic_module(scope, module_name, &export_strings, evaluate)
}

fn export_names() -> &'static [&'static str] {
    &[
        "format",
        "formatWithOptions",
        "inspect",
        "promisify",
        "callbackify",
        "deprecate",
        "types",
        "isDeepStrictEqual",
        "parseArgs",
        "TextEncoder",
        "TextDecoder",
        "default",
    ]
}

fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);

    // Compile + run the JS body to produce a namespace object literal,
    // then mirror its properties onto the module's named exports.
    let src = v8::String::new(scope, JS_SOURCE).unwrap();
    let script = v8::Script::compile(scope, src, None)?;
    let ns_val = script.run(scope)?;
    let ns = v8::Local::<v8::Object>::try_from(ns_val).ok()?;

    for name in export_names() {
        if *name == "default" {
            continue;
        }
        let key = v8::String::new(scope, name).unwrap();
        let val = ns
            .get(scope, key.into())
            .unwrap_or_else(|| v8::undefined(scope).into());
        let _ = module.set_synthetic_module_export(scope, key, val);
    }
    let default_key = v8::String::new(scope, "default").unwrap();
    let _ = module.set_synthetic_module_export(scope, default_key, ns.into());

    Some(v8::undefined(scope).into())
}

/// Pure-JS body. Evaluated once per `import` of `node:util`; returns
/// the namespace object the synthetic module mirrors into named
/// exports.
const JS_SOURCE: &str = r#"
(() => {
    const objToString = Object.prototype.toString;
    const tag = (v) => objToString.call(v).slice(8, -1);

    // ── format / formatWithOptions ──────────────────────────────────────
    // printf-style. Supports %s %d %i %f %j %o %O %c %%. Extra args are
    // appended space-separated (Node behavior). %o/%O delegate to inspect.
    function formatWithOptions(opts, fmt, ...rest) {
        if (typeof fmt !== "string") {
            const parts = [fmt, ...rest].map((v) =>
                typeof v === "string" ? v : inspect(v, opts)
            );
            return parts.join(" ");
        }
        let out = "";
        let i = 0;
        let argIdx = 0;
        while (i < fmt.length) {
            const ch = fmt[i];
            if (ch === "%" && i + 1 < fmt.length) {
                const spec = fmt[i + 1];
                if (spec === "%") { out += "%"; i += 2; continue; }
                if (argIdx >= rest.length) { out += "%" + spec; i += 2; continue; }
                const a = rest[argIdx++];
                switch (spec) {
                    case "s":
                        out += (a == null) ? String(a) :
                               (typeof a === "object" || typeof a === "symbol") ? inspect(a, opts) :
                               String(a);
                        break;
                    case "d":
                    case "i": {
                        if (typeof a === "bigint") { out += String(a) + "n"; break; }
                        if (typeof a === "symbol") { out += "NaN"; break; }
                        out += String(spec === "i" ? Math.trunc(Number(a)) : Number(a));
                        break;
                    }
                    case "f":
                        out += (typeof a === "symbol") ? "NaN" : String(Number(a));
                        break;
                    case "j":
                        try { out += JSON.stringify(a); }
                        catch (e) { out += (e && e.message && e.message.includes("circular")) ? "[Circular]" : "[JSON Error]"; }
                        break;
                    case "o":
                    case "O":
                        out += inspect(a, opts);
                        break;
                    case "c":
                        // CSS spec — Node ignores; consume the arg.
                        break;
                    default:
                        out += "%" + spec; argIdx--; break;
                }
                i += 2;
            } else {
                out += ch; i++;
            }
        }
        // Append any remaining args separated by spaces.
        while (argIdx < rest.length) {
            const a = rest[argIdx++];
            out += " " + (typeof a === "string" ? a : inspect(a, opts));
        }
        return out;
    }
    function format(fmt, ...rest) { return formatWithOptions({}, fmt, ...rest); }

    // ── inspect ─────────────────────────────────────────────────────────
    // Minimal viable: primitives, arrays, plain objects, Map, Set, Date,
    // RegExp, Error; circular detection via seen-WeakSet; max depth.
    // Deferred: util.inspect.custom symbol, Symbol keys, getters,
    // typed-arrays-as-hex, BigInt n-suffix from %s.
    function inspect(value, options) {
        const opts = Object.assign({ depth: 2, breakLength: 80, maxArrayLength: 100 }, options || {});
        const seen = new WeakSet();
        return _inspect(value, opts, 0, seen);
    }
    function _inspect(v, opts, depth, seen) {
        if (v === null) return "null";
        if (v === undefined) return "undefined";
        const t = typeof v;
        if (t === "string") return JSON.stringify(v);
        if (t === "number" || t === "boolean") return String(v);
        if (t === "bigint") return String(v) + "n";
        if (t === "symbol") return v.toString();
        if (t === "function") {
            const n = v.name || "(anonymous)";
            return v.constructor && v.constructor.name === "AsyncFunction"
                ? `[AsyncFunction: ${n}]`
                : `[Function: ${n}]`;
        }
        // Object-ish from here.
        if (seen.has(v)) return "[Circular]";
        seen.add(v);
        if (opts.depth != null && depth > opts.depth) {
            if (Array.isArray(v)) return "[Array]";
            const tg = tag(v);
            return tg === "Object" ? "[Object]" : `[${tg}]`;
        }
        if (v instanceof Date) return v.toISOString();
        if (v instanceof RegExp) return v.toString();
        if (v instanceof Error) {
            const m = v.message || "";
            return v.stack || `${v.name || "Error"}: ${m}`;
        }
        if (v instanceof Map) {
            const items = [];
            let i = 0;
            for (const [k, val] of v) {
                if (i++ >= opts.maxArrayLength) { items.push("..."); break; }
                items.push(_inspect(k, opts, depth + 1, seen) + " => " + _inspect(val, opts, depth + 1, seen));
            }
            return `Map(${v.size}) { ${items.join(", ")} }`;
        }
        if (v instanceof Set) {
            const items = [];
            let i = 0;
            for (const val of v) {
                if (i++ >= opts.maxArrayLength) { items.push("..."); break; }
                items.push(_inspect(val, opts, depth + 1, seen));
            }
            return `Set(${v.size}) { ${items.join(", ")} }`;
        }
        if (Array.isArray(v)) {
            const items = [];
            const n = Math.min(v.length, opts.maxArrayLength);
            for (let i = 0; i < n; i++) {
                items.push(_inspect(v[i], opts, depth + 1, seen));
            }
            if (v.length > n) items.push(`... ${v.length - n} more items`);
            return "[ " + items.join(", ") + " ]";
        }
        // Plain object (or unknown class).
        const keys = Object.keys(v);
        const ctor = v.constructor && v.constructor.name;
        const prefix = (ctor && ctor !== "Object") ? ctor + " " : "";
        if (keys.length === 0) return prefix + "{}";
        const items = keys.map((k) => {
            const safe = /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(k) ? k : JSON.stringify(k);
            return safe + ": " + _inspect(v[k], opts, depth + 1, seen);
        });
        return prefix + "{ " + items.join(", ") + " }";
    }
    // Stubbed color tables — Node ships terminal ANSI color metadata
    // here. We expose the shape so consumers that probe `inspect.colors`
    // don't crash; styleText is deferred until terminal-support lands.
    inspect.colors = Object.create(null);
    inspect.styles = Object.create(null);
    inspect.defaultOptions = { depth: 2, breakLength: 80, maxArrayLength: 100 };
    inspect.custom = Symbol.for("nodejs.util.inspect.custom");

    // ── promisify / callbackify ─────────────────────────────────────────
    const kCustom = Symbol.for("util.promisify.custom");
    function promisify(fn) {
        if (typeof fn !== "function") throw new TypeError("promisify: argument must be a function");
        if (fn[kCustom]) return fn[kCustom];
        function wrapped(...args) {
            return new Promise((resolve, reject) => {
                try {
                    fn.call(this, ...args, (err, ...res) => {
                        if (err) reject(err);
                        else resolve(res.length > 1 ? res : res[0]);
                    });
                } catch (e) { reject(e); }
            });
        }
        Object.setPrototypeOf(wrapped, Object.getPrototypeOf(fn));
        return wrapped;
    }
    promisify.custom = kCustom;
    function callbackify(fn) {
        if (typeof fn !== "function") throw new TypeError("callbackify: argument must be a function");
        function wrapped(...args) {
            const cb = args.pop();
            if (typeof cb !== "function") throw new TypeError("callbackify: last argument must be a callback");
            Promise.resolve().then(() => fn.apply(this, args)).then(
                (v) => cb(null, v),
                (e) => cb(e || new Error("falsy rejection")),
            );
        }
        Object.setPrototypeOf(wrapped, Object.getPrototypeOf(fn));
        return wrapped;
    }

    // ── deprecate ───────────────────────────────────────────────────────
    function deprecate(fn, msg, code) {
        let warned = false;
        function deprecated(...args) {
            if (!warned) {
                warned = true;
                const prefix = code ? `[${code}] ` : "";
                try { console.warn("DeprecationWarning: " + prefix + msg); } catch (_) {}
            }
            return fn.apply(this, args);
        }
        Object.setPrototypeOf(deprecated, Object.getPrototypeOf(fn));
        return deprecated;
    }

    // ── types.* predicates ──────────────────────────────────────────────
    // Strategy: brand-check via Object.prototype.toString for spec'd
    // internal slots; instanceof for ergonomic JS classes.
    const types = Object.freeze({
        isAnyArrayBuffer: (v) => tag(v) === "ArrayBuffer" || tag(v) === "SharedArrayBuffer",
        isArrayBuffer: (v) => tag(v) === "ArrayBuffer",
        isSharedArrayBuffer: (v) => tag(v) === "SharedArrayBuffer",
        isArgumentsObject: (v) => tag(v) === "Arguments",
        isAsyncFunction: (v) => tag(v) === "AsyncFunction",
        isBigInt64Array: (v) => tag(v) === "BigInt64Array",
        isBigUint64Array: (v) => tag(v) === "BigUint64Array",
        isBooleanObject: (v) => v instanceof Boolean,
        isBoxedPrimitive: (v) => v instanceof Boolean || v instanceof Number || v instanceof String || v instanceof Symbol || v instanceof BigInt,
        isDataView: (v) => tag(v) === "DataView",
        isDate: (v) => tag(v) === "Date",
        isFloat32Array: (v) => tag(v) === "Float32Array",
        isFloat64Array: (v) => tag(v) === "Float64Array",
        isGeneratorFunction: (v) => tag(v) === "GeneratorFunction",
        isGeneratorObject: (v) => tag(v) === "Generator",
        isInt8Array: (v) => tag(v) === "Int8Array",
        isInt16Array: (v) => tag(v) === "Int16Array",
        isInt32Array: (v) => tag(v) === "Int32Array",
        isMap: (v) => tag(v) === "Map",
        isMapIterator: (v) => tag(v) === "Map Iterator",
        isModuleNamespaceObject: (v) => v != null && typeof v === "object" && v[Symbol.toStringTag] === "Module",
        isNativeError: (v) => v instanceof Error,
        isNumberObject: (v) => v instanceof Number,
        isPromise: (v) => tag(v) === "Promise",
        isProxy: (_) => false, // no spec way to detect a Proxy from JS.
        isRegExp: (v) => tag(v) === "RegExp",
        isSet: (v) => tag(v) === "Set",
        isSetIterator: (v) => tag(v) === "Set Iterator",
        isStringObject: (v) => v instanceof String,
        isSymbolObject: (v) => typeof v === "object" && v !== null && tag(v) === "Symbol",
        isTypedArray: (v) => ArrayBuffer.isView(v) && tag(v) !== "DataView",
        isUint8Array: (v) => tag(v) === "Uint8Array",
        isUint8ClampedArray: (v) => tag(v) === "Uint8ClampedArray",
        isUint16Array: (v) => tag(v) === "Uint16Array",
        isUint32Array: (v) => tag(v) === "Uint32Array",
        isWeakMap: (v) => tag(v) === "WeakMap",
        isWeakSet: (v) => tag(v) === "WeakSet",
    });

    // ── isDeepStrictEqual ───────────────────────────────────────────────
    // Recursive structural equal. Skips Map/Set ordering quirks: key
    // sets must match; insertion order is not enforced past Map keys
    // present in both.
    function isDeepStrictEqual(a, b) {
        return _deepEq(a, b, new Map());
    }
    function _deepEq(a, b, memo) {
        if (Object.is(a, b)) return true;
        if (typeof a !== "object" || a === null || typeof b !== "object" || b === null) return false;
        if (Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
        // Memoize visited pairs to terminate on cycles.
        const seen = memo.get(a);
        if (seen && seen.has(b)) return true;
        if (seen) seen.add(b); else memo.set(a, new Set([b]));
        const ta = tag(a), tb = tag(b);
        if (ta !== tb) return false;
        if (ta === "Date") return a.getTime() === b.getTime();
        if (ta === "RegExp") return a.source === b.source && a.flags === b.flags;
        if (ArrayBuffer.isView(a) && ta !== "DataView") {
            if (a.length !== b.length) return false;
            for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
            return true;
        }
        if (ta === "ArrayBuffer") {
            if (a.byteLength !== b.byteLength) return false;
            const va = new Uint8Array(a), vb = new Uint8Array(b);
            for (let i = 0; i < va.length; i++) if (va[i] !== vb[i]) return false;
            return true;
        }
        if (Array.isArray(a)) {
            if (a.length !== b.length) return false;
            for (let i = 0; i < a.length; i++) if (!_deepEq(a[i], b[i], memo)) return false;
            return true;
        }
        if (ta === "Map") {
            if (a.size !== b.size) return false;
            for (const [k, v] of a) {
                if (!b.has(k) || !_deepEq(v, b.get(k), memo)) return false;
            }
            return true;
        }
        if (ta === "Set") {
            if (a.size !== b.size) return false;
            for (const v of a) if (!b.has(v)) return false;
            return true;
        }
        const ka = Object.keys(a), kb = Object.keys(b);
        if (ka.length !== kb.length) return false;
        for (const k of ka) {
            if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
            if (!_deepEq(a[k], b[k], memo)) return false;
        }
        return true;
    }

    // ── parseArgs (minimal) ─────────────────────────────────────────────
    // Long-form `--flag` and `--flag value`. Boolean if option type is
    // "boolean" or omitted; string if "string". Positionals collected
    // in `.positionals`. Deferred: short flags, `--flag=value`,
    // multiples, `tokens`, strict mode, allowPositionals.
    function parseArgs(config) {
        const cfg = config || {};
        const args = cfg.args || [];
        const options = cfg.options || {};
        const values = Object.create(null);
        const positionals = [];
        for (const k of Object.keys(options)) {
            if (options[k].multiple) values[k] = [];
        }
        for (let i = 0; i < args.length; i++) {
            const a = args[i];
            if (typeof a !== "string") { positionals.push(a); continue; }
            if (a.startsWith("--")) {
                let name, val;
                const eq = a.indexOf("=");
                if (eq >= 0) { name = a.slice(2, eq); val = a.slice(eq + 1); }
                else { name = a.slice(2); val = undefined; }
                const opt = options[name] || { type: "boolean" };
                if (opt.type === "string") {
                    if (val === undefined) {
                        if (i + 1 >= args.length) throw new Error(`Option '--${name}' requires a value`);
                        val = args[++i];
                    }
                    if (opt.multiple) values[name].push(val);
                    else values[name] = val;
                } else {
                    // boolean
                    if (opt.multiple) values[name].push(true);
                    else values[name] = val === undefined ? true : (val !== "false");
                }
            } else {
                positionals.push(a);
            }
        }
        return { values, positionals };
    }

    // ── TextEncoder / TextDecoder re-export ─────────────────────────────
    // The runtime installs these as native classes on globalThis; we
    // expose the same identities here so `node:util`'s constructor is
    // === globalThis.TextEncoder.
    const TextEncoder = globalThis.TextEncoder;
    const TextDecoder = globalThis.TextDecoder;

    return {
        format, formatWithOptions, inspect,
        promisify, callbackify, deprecate,
        types, isDeepStrictEqual, parseArgs,
        TextEncoder, TextDecoder,
    };
})()
"#;
