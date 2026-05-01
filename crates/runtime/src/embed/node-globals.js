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
        // Encoding-aware toString matching Node's Buffer surface. Vite's
        // ModuleRunner does `Buffer.from(b64, "base64").toString()` on
        // inline sourcemaps; without proper toString the read falls back
        // to Uint8Array.prototype.toString() (comma-separated decimals)
        // and JSON.parse explodes downstream.
        const bufProto = Object.create(Uint8Array.prototype);
        bufProto.toString = function (encoding, start, end) {
          var s = start == null ? 0 : start;
          var e = end == null ? this.length : end;
          var sub = this.subarray(s, e);
          var enc = encoding == null ? "utf8" : String(encoding).toLowerCase();
          if (enc === "utf8" || enc === "utf-8") {
            return new TextDecoder().decode(sub);
          }
          if (enc === "base64") {
            var bin = "";
            for (var i = 0; i < sub.length; i++) bin += String.fromCharCode(sub[i]);
            return btoa(bin);
          }
          if (enc === "base64url") {
            var bin = "";
            for (var i = 0; i < sub.length; i++) bin += String.fromCharCode(sub[i]);
            return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
          }
          if (enc === "hex") {
            var hex = "";
            for (var i = 0; i < sub.length; i++) {
              hex += (sub[i] < 16 ? "0" : "") + sub[i].toString(16);
            }
            return hex;
          }
          if (enc === "ascii" || enc === "latin1" || enc === "binary") {
            var bin = "";
            for (var i = 0; i < sub.length; i++) bin += String.fromCharCode(sub[i] & 0x7f);
            return bin;
          }
          return new TextDecoder().decode(sub);
        };

        function asBuf(u8) {
          Object.setPrototypeOf(u8, bufProto);
          return u8;
        }

        const stub = function (...args) {
          if (typeof args[0] === "number") return asBuf(new Uint8Array(args[0]));
          return asBuf(Uint8Array.from(args[0] ?? []));
        };
        stub.from = (input, encoding) => {
          if (typeof input === "string") {
            if (encoding === "base64" || encoding === "base64url") {
              var b64 = encoding === "base64url"
                ? input.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((input.length + 3) % 4)
                : input;
              var bin = atob(b64);
              var out = new Uint8Array(bin.length);
              for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
              return asBuf(out);
            }
            if (encoding === "hex") {
              var len = input.length >>> 1;
              var out = new Uint8Array(len);
              for (var i = 0; i < len; i++) out[i] = parseInt(input.substr(i * 2, 2), 16);
              return asBuf(out);
            }
            return asBuf(new TextEncoder().encode(input));
          }
          if (input instanceof ArrayBuffer) return asBuf(new Uint8Array(input));
          if (input && typeof input.byteLength === "number" && input.buffer instanceof ArrayBuffer) {
            return asBuf(new Uint8Array(input.buffer, input.byteOffset, input.byteLength));
          }
          return asBuf(Uint8Array.from(input));
        };
        stub.alloc = (size, fill = 0) => asBuf(new Uint8Array(size).fill(fill));
        stub.allocUnsafe = (size) => asBuf(new Uint8Array(size));
        stub.concat = (list, totalLen) => {
          var len = totalLen == null ? list.reduce((s, b) => s + b.length, 0) : totalLen;
          var out = new Uint8Array(len);
          var off = 0;
          for (const b of list) { out.set(b, off); off += b.length; }
          return asBuf(out);
        };
        stub.isBuffer = (x) => x != null && Object.getPrototypeOf(x) === bufProto;
        stub.byteLength = (s, encoding) => {
          if (typeof s !== "string") return s.byteLength ?? s.length;
          if (encoding === "base64") return Math.floor(s.length * 3 / 4);
          if (encoding === "hex") return s.length >>> 1;
          return new TextEncoder().encode(s).length;
        };
        stub.prototype = bufProto;
        bufProto.constructor = stub;
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
