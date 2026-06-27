//! Native `node:buffer`.
//!
//! Migrates `import { Buffer } from "node:buffer"` from unenv's polyfill
//! to a native synthetic module + JS-side `class Buffer extends
//! Uint8Array`. The class body is compiled once into `BUFFER_JS` and
//! rerun in each isolate at module-evaluation time so the constructor,
//! prototype, and statics are isolate-local.
//!
//! ## Implementation choice
//!
//! Pure JS class extending `Uint8Array` (Option B in the design notes).
//! `#[v8_class]` doesn't model subclassing of a V8 built-in — Buffer
//! must inherit `Uint8Array.prototype` while owning Node's static
//! methods (alloc / from / byteLength / concat / compare / isBuffer /
//! …) and prototype methods (toString / write / fill / read*/write*
//! numerics / toJSON / …). A JS body lets V8's JIT inline the hot
//! paths (`from(string)`, `toString()`, the typed-array reads/writes)
//! against TypedArray.prototype primitives directly, without an FFI
//! hop per call.
//!
//! ## Globals
//!
//! Node exposes `Buffer` on the global object so app code can use it
//! without `import { Buffer } from "node:buffer"`. The bare `Buffer`
//! global is the same constructor as the module export (one isolate-
//! local class object). `install_global` from `core::init` runs after
//! the synthetic module has been evaluated.
//!
//! ## Encodings
//!
//! `utf8` (default), `utf-8`, `utf16le` / `utf-16le` / `ucs2` / `ucs-2`,
//! `latin1` / `binary`, `ascii`, `hex`, `base64`, `base64url`. Mirrors
//! `node/crypto/encoding.rs` — same alias table, same case-insensitive
//! match.
//!
//! ## Deferred
//!
//! - `Buffer.transcode(source, fromEnc, toEnc)` — needs full ICU.
//! - `Buffer.constants.MAX_STRING_LENGTH` — V8-internal, undefined.
//! - `resolveObjectURL`, `transferAsUncopied` — Node-internals.
//!
//! `atob` / `btoa` already live on `globalThis` via `web/base64.rs` —
//! we don't shadow them.

#![allow(unsafe_code)]

mod encoding;
mod native;

pub use native::{
    emit_buffer, emit_output, emit_string, emit_uint8array, extract_input,
};

/// Mint a synthetic ESM record for `node:buffer`. Called from
/// `core::native_modules::resolve_native`.
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:buffer").unwrap();
    let names = export_names();
    let export_strings: Vec<v8::Local<v8::String>> = names
        .iter()
        .map(|n| v8::String::new(scope, n).unwrap())
        .collect();
    v8::Module::create_synthetic_module(scope, module_name, &export_strings, evaluate)
}

fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);

    let ns = build_namespace(scope)?;

    for name in export_names() {
        if *name == "default" { continue; }
        let key = v8::String::new(scope, name).unwrap();
        let val = ns.get(scope, key.into()).unwrap_or_else(|| v8::undefined(scope).into());
        let _ = module.set_synthetic_module_export(scope, key, val);
    }
    let default_key = v8::String::new(scope, "default").unwrap();
    let _ = module.set_synthetic_module_export(scope, default_key, ns.into());

    Some(v8::undefined(scope).into())
}

fn export_names() -> &'static [&'static str] {
    &[
        "Buffer",
        "kMaxLength",
        "INSPECT_MAX_BYTES",
        "constants",
        "atob",
        "btoa",
        "isUtf8",
        "isAscii",
        "default",
    ]
}

/// Build (or fetch from the isolate slot) the namespace object that
/// holds Buffer + statics. Returns `None` on JS compile failure (the
/// V8 exception bubbles up through `evaluate`'s module return).
fn build_namespace<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Object>> {
    // Compile + run BUFFER_JS once per isolate; the result is an object
    // literal `{ Buffer, kMaxLength, ... }`. Stash it under a private
    // global key so re-evaluation (e.g. dynamic re-import) returns the
    // same constructor — instanceof checks across module boundaries
    // need a single class identity.
    let context = scope.get_current_context();
    let global = context.global(scope);
    let key = v8::String::new(scope, "__zsBufferNs").unwrap();

    // Fast path: already built.
    if let Some(existing) = global.get(scope, key.into()) {
        if existing.is_object() {
            return v8::Local::<v8::Object>::try_from(existing).ok();
        }
    }

    // Slow path: compile + run the JS body.
    let src = v8::String::new(scope, BUFFER_JS).unwrap();
    let script = v8::Script::compile(scope, src, None)?;
    let result = script.run(scope)?;
    let obj = v8::Local::<v8::Object>::try_from(result).ok()?;

    // Stash under a non-enumerable property; the bridge user-facing
    // Buffer global is set by `install_global` separately.
    global.set(scope, key.into(), obj.into());
    Some(obj)
}

/// Install `globalThis.Buffer` (Node convention — bare `Buffer.from(...)`
/// must work without an explicit import). Same constructor identity as
/// the `node:buffer` module export.
///
/// Called from `core::init::load_polyfills_and_modules` after the
/// SyntheticModule registry is wired up.
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let ns = match build_namespace(scope) {
        Some(o) => o,
        None => return, // BUFFER_JS compile failed — let the test surface it.
    };
    let buffer_key = v8::String::new(scope, "Buffer").unwrap();
    if let Some(buf_ctor) = ns.get(scope, buffer_key.into()) {
        global.set(scope, buffer_key.into(), buf_ctor);
    }
}

// ---------------------------------------------------------------------------
// JS body — compiled once per isolate
// ---------------------------------------------------------------------------

/// JS source for the Buffer class. Returns an object holding the Buffer
/// constructor + module-level statics + helpers. Run inside V8 via
/// `v8::Script::compile`.
///
/// Layout (everything in one IIFE so internals don't leak to globalThis):
///
///   ({
///     Buffer,            // class extends Uint8Array
///     kMaxLength,        // 0xffffffff (Node 22)
///     INSPECT_MAX_BYTES, // 50 (Node default)
///     constants,         // { MAX_LENGTH, MAX_STRING_LENGTH? }
///     atob, btoa,        // re-export the WHATWG globals (Node convention)
///     isUtf8, isAscii,   // typed-array introspection helpers
///   })
///
/// Encoding semantics: utf8 (default), utf-8 (alias), utf16le / utf-16le
/// / ucs2 / ucs-2, latin1 / binary, ascii (high bit stripped per Node),
/// hex (tolerant of odd length / whitespace), base64 (forgiving),
/// base64url. Matches `node/crypto/encoding.rs`.
///
/// Read*/write* numerics are generated via a tiny in-script DataView
/// loop so the file stays under 500 lines without macro pyrotechnics.
const BUFFER_JS: &str = r#"
(() => {
  // -------------------------------------------------------------
  // Encoding registry — case-insensitive aliases.
  // -------------------------------------------------------------
  const ENCODINGS = new Set([
    "utf8", "utf-8",
    "utf16le", "utf-16le", "ucs2", "ucs-2",
    "latin1", "binary",
    "ascii",
    "hex",
    "base64", "base64url",
  ]);
  const normEnc = (e) => {
    if (e == null) return "utf8";
    const s = String(e).toLowerCase();
    if (s === "utf-8") return "utf8";
    if (s === "utf-16le" || s === "ucs2" || s === "ucs-2") return "utf16le";
    if (s === "binary") return "latin1";
    return s;
  };
  const isEncoding = (e) => {
    if (typeof e !== "string") return false;
    return ENCODINGS.has(e.toLowerCase());
  };

  // ----- bytes <-> string per encoding --------------------------
  const TE = new TextEncoder();
  const TD = new TextDecoder("utf-8", { fatal: false });

  const HEX_TABLE = "0123456789abcdef";

  function bytesFromString(s, encoding) {
    encoding = normEnc(encoding);
    switch (encoding) {
      case "utf8": return TE.encode(s);
      case "utf16le": {
        const out = new Uint8Array(s.length * 2);
        for (let i = 0; i < s.length; i++) {
          const cu = s.charCodeAt(i);
          out[i * 2] = cu & 0xff;
          out[i * 2 + 1] = (cu >> 8) & 0xff;
        }
        return out;
      }
      case "latin1": {
        const out = new Uint8Array(s.length);
        for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i) & 0xff;
        return out;
      }
      case "ascii": {
        const out = new Uint8Array(s.length);
        for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i) & 0x7f;
        return out;
      }
      case "hex": {
        // Tolerant: skip whitespace, halt at odd nibble.
        const out = [];
        let nib = -1;
        for (let i = 0; i < s.length; i++) {
          const c = s.charCodeAt(i);
          let v;
          if (c >= 48 && c <= 57) v = c - 48;
          else if (c >= 97 && c <= 102) v = c - 87;
          else if (c >= 65 && c <= 70) v = c - 55;
          else continue;
          if (nib < 0) nib = v; else { out.push((nib << 4) | v); nib = -1; }
        }
        return Uint8Array.from(out);
      }
      case "base64":
      case "base64url": {
        // Re-use the WHATWG `atob` after normalising URL-safe alphabet
        // and padding. atob is forgiving of trailing whitespace + opt
        // padding (we strip non-alphabet chars and pad here too so
        // base64url without padding round-trips).
        let cleaned = "";
        for (let i = 0; i < s.length; i++) {
          const c = s[i];
          if (c === "-") cleaned += "+";
          else if (c === "_") cleaned += "/";
          else if (c === "=" || /[A-Za-z0-9+/]/.test(c)) cleaned += c;
        }
        // Strip trailing `=`, then re-pad to multiple of 4.
        cleaned = cleaned.replace(/=+$/, "");
        while (cleaned.length % 4 !== 0) cleaned += "=";
        let bin;
        try { bin = atob(cleaned); } catch { return new Uint8Array(0); }
        const out = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
        return out;
      }
      default:
        throw new TypeError(`Unknown encoding: ${encoding}`);
    }
  }

  function bytesToString(view, encoding, start, end) {
    encoding = normEnc(encoding);
    const len = view.length;
    start = start === undefined ? 0 : Math.max(0, Math.min(len, start | 0));
    end = end === undefined ? len : Math.max(0, Math.min(len, end | 0));
    if (end < start) end = start;
    const slice = view.subarray(start, end);
    switch (encoding) {
      case "utf8": return TD.decode(slice);
      case "utf16le": {
        // Each pair → one code unit (LE).
        let s = "";
        const pairs = slice.length >>> 1;
        for (let i = 0; i < pairs; i++) {
          s += String.fromCharCode(slice[i * 2] | (slice[i * 2 + 1] << 8));
        }
        return s;
      }
      case "latin1": {
        let s = "";
        for (let i = 0; i < slice.length; i++) s += String.fromCharCode(slice[i]);
        return s;
      }
      case "ascii": {
        let s = "";
        for (let i = 0; i < slice.length; i++) s += String.fromCharCode(slice[i] & 0x7f);
        return s;
      }
      case "hex": {
        let s = "";
        for (let i = 0; i < slice.length; i++) {
          const b = slice[i];
          s += HEX_TABLE[b >> 4] + HEX_TABLE[b & 0xf];
        }
        return s;
      }
      case "base64":
      case "base64url": {
        // btoa wants a Latin-1 string; build it from bytes then encode.
        let bin = "";
        for (let i = 0; i < slice.length; i++) bin += String.fromCharCode(slice[i]);
        let s = btoa(bin);
        if (encoding === "base64url") {
          s = s.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
        }
        return s;
      }
      default:
        throw new TypeError(`Unknown encoding: ${encoding}`);
    }
  }

  // -------------------------------------------------------------
  // Buffer class
  // -------------------------------------------------------------
  class Buffer extends Uint8Array {
    // `Uint8Array` subclasses are constructed via the same arg
    // forms (number | TypedArray | ArrayBuffer | iterable). Node's
    // `new Buffer(x)` is *deprecated*; we keep it working as a thin
    // alias for `Buffer.from(x)` because the deprecated path is what
    // npm-bundled code shipping for older Node versions still hits.
    constructor(arg, encodingOrOffset, length) {
      if (typeof arg === "number") {
        super(arg);
      } else if (typeof arg === "string") {
        const bytes = bytesFromString(arg, encodingOrOffset);
        super(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      } else if (arg instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && arg instanceof SharedArrayBuffer)) {
        const offset = encodingOrOffset || 0;
        const len = length === undefined ? arg.byteLength - offset : length;
        super(arg, offset, len);
      } else if (ArrayBuffer.isView(arg)) {
        super(arg.buffer, arg.byteOffset, arg.byteLength);
      } else if (arg && typeof arg[Symbol.iterator] === "function") {
        super(Uint8Array.from(arg));
      } else if (arg && typeof arg.length === "number") {
        super(Uint8Array.from(arg));
      } else {
        super(0);
      }
    }

    // ----- Static factories -------------------------------------
    static alloc(size, fill, encoding) {
      const b = new Buffer(size | 0);
      if (fill !== undefined && fill !== 0) b.fill(fill, 0, b.length, encoding);
      return b;
    }
    static allocUnsafe(size) {
      // V8 zero-fills TypedArrays — there's no unsafe path. We keep
      // the Node API for source-compat.
      return new Buffer(size | 0);
    }
    static allocUnsafeSlow(size) { return new Buffer(size | 0); }

    static from(value, encodingOrOffset, length) {
      if (value == null) {
        throw new TypeError("The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object");
      }
      if (typeof value === "string") {
        const bytes = bytesFromString(value, encodingOrOffset);
        // Wrap the freshly-allocated buffer so it's a Buffer, not Uint8Array.
        return new Buffer(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      }
      if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer)) {
        const offset = encodingOrOffset || 0;
        const len = length === undefined ? value.byteLength - offset : length;
        return new Buffer(value, offset, len);
      }
      if (ArrayBuffer.isView(value)) {
        // Copy bytes (Node convention — `Buffer.from(typedArray)` copies).
        const out = new Buffer(value.byteLength);
        const src = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        out.set(src);
        return out;
      }
      if (Array.isArray(value) || (value && typeof value[Symbol.iterator] === "function")) {
        return new Buffer(Uint8Array.from(value));
      }
      if (value && typeof value === "object" && value.type === "Buffer" && Array.isArray(value.data)) {
        // toJSON round-trip shape.
        return new Buffer(Uint8Array.from(value.data));
      }
      if (value && typeof value.length === "number") {
        return new Buffer(Uint8Array.from(value));
      }
      throw new TypeError("The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object");
    }

    static of(...bytes) { return new Buffer(Uint8Array.of(...bytes)); }

    static byteLength(string, encoding) {
      if (typeof string !== "string") {
        if (ArrayBuffer.isView(string)) return string.byteLength;
        if (string instanceof ArrayBuffer) return string.byteLength;
        throw new TypeError("byteLength: input must be a string, Buffer, or ArrayBuffer");
      }
      return bytesFromString(string, encoding).length;
    }

    static compare(a, b) {
      if (!(a instanceof Uint8Array) || !(b instanceof Uint8Array)) {
        throw new TypeError("compare: both arguments must be Buffers or Uint8Arrays");
      }
      const min = Math.min(a.length, b.length);
      for (let i = 0; i < min; i++) {
        if (a[i] < b[i]) return -1;
        if (a[i] > b[i]) return 1;
      }
      if (a.length < b.length) return -1;
      if (a.length > b.length) return 1;
      return 0;
    }

    static concat(list, totalLength) {
      if (!Array.isArray(list)) throw new TypeError("concat: list must be an array");
      if (totalLength === undefined) {
        totalLength = 0;
        for (const b of list) totalLength += b.length;
      }
      const out = Buffer.alloc(totalLength | 0);
      let pos = 0;
      for (const b of list) {
        if (pos >= totalLength) break;
        const take = Math.min(b.length, totalLength - pos);
        out.set(b.subarray(0, take), pos);
        pos += take;
      }
      return out;
    }

    static isBuffer(obj) { return obj instanceof Buffer; }
    static isEncoding(name) { return isEncoding(name); }

    static copyBytesFrom(view, offset, length) {
      if (!ArrayBuffer.isView(view)) throw new TypeError("copyBytesFrom: view must be a TypedArray");
      const elementSize = view.BYTES_PER_ELEMENT || 1;
      offset = offset === undefined ? 0 : offset | 0;
      length = length === undefined ? view.length - offset : length | 0;
      const start = view.byteOffset + offset * elementSize;
      const byteLen = length * elementSize;
      const out = Buffer.alloc(byteLen);
      const src = new Uint8Array(view.buffer, start, byteLen);
      out.set(src);
      return out;
    }

    // ----- Prototype methods ------------------------------------
    toString(encoding, start, end) {
      return bytesToString(this, encoding, start, end);
    }

    toJSON() {
      const data = new Array(this.length);
      for (let i = 0; i < this.length; i++) data[i] = this[i];
      return { type: "Buffer", data };
    }

    write(string, offset, length, encoding) {
      // Variants: write(s) / write(s, enc) / write(s, off, enc) /
      // write(s, off, len, enc).
      if (typeof offset === "string") { encoding = offset; offset = 0; length = this.length; }
      else if (typeof length === "string") { encoding = length; length = this.length - (offset | 0); }
      offset = offset | 0;
      const remaining = this.length - offset;
      if (remaining <= 0) return 0;
      if (length === undefined || length > remaining) length = remaining;
      const bytes = bytesFromString(string, encoding);
      const n = Math.min(bytes.length, length);
      for (let i = 0; i < n; i++) this[offset + i] = bytes[i];
      return n;
    }

    fill(value, offset, end, encoding) {
      if (offset === undefined) { offset = 0; end = this.length; }
      else if (typeof offset === "string") { encoding = offset; offset = 0; end = this.length; }
      else if (typeof end === "string") { encoding = end; end = this.length; }
      offset = offset | 0;
      end = end === undefined ? this.length : (end | 0);
      if (offset < 0 || end > this.length || offset > end) {
        throw new RangeError("fill: out of range");
      }
      let pattern;
      if (typeof value === "number") {
        Uint8Array.prototype.fill.call(this, value & 0xff, offset, end);
        return this;
      }
      if (typeof value === "string") {
        pattern = bytesFromString(value, encoding);
      } else if (value instanceof Uint8Array) {
        pattern = value;
      } else {
        // Empty / unrecognised → zero-fill.
        Uint8Array.prototype.fill.call(this, 0, offset, end);
        return this;
      }
      if (pattern.length === 0) {
        Uint8Array.prototype.fill.call(this, 0, offset, end);
        return this;
      }
      // Tile the pattern across [offset, end).
      let i = offset;
      while (i < end) {
        const take = Math.min(pattern.length, end - i);
        for (let j = 0; j < take; j++) this[i + j] = pattern[j];
        i += take;
      }
      return this;
    }

    copy(target, targetStart, sourceStart, sourceEnd) {
      targetStart = targetStart | 0;
      sourceStart = sourceStart | 0;
      sourceEnd = sourceEnd === undefined ? this.length : sourceEnd | 0;
      if (sourceEnd > this.length) sourceEnd = this.length;
      if (targetStart >= target.length || sourceStart >= sourceEnd) return 0;
      const len = Math.min(sourceEnd - sourceStart, target.length - targetStart);
      target.set(this.subarray(sourceStart, sourceStart + len), targetStart);
      return len;
    }

    // Node's `subarray` returns a Buffer (NOT a Uint8Array). The
    // built-in Uint8Array.subarray honours `Symbol.species`, but only
    // when set on the constructor — we override directly so the
    // spread behaves predictably across V8 versions.
    subarray(start, end) {
      const view = Uint8Array.prototype.subarray.call(this, start, end);
      // Re-tag the view as a Buffer (no copy, shared backing store).
      Object.setPrototypeOf(view, Buffer.prototype);
      return view;
    }
    // `slice` is deprecated (Node 24+) but still ubiquitous.
    slice(start, end) { return this.subarray(start, end); }

    equals(other) {
      if (!(other instanceof Uint8Array)) throw new TypeError("equals: argument must be a Buffer or Uint8Array");
      if (other.length !== this.length) return false;
      for (let i = 0; i < this.length; i++) if (this[i] !== other[i]) return false;
      return true;
    }

    compare(target, targetStart, targetEnd, sourceStart, sourceEnd) {
      if (!(target instanceof Uint8Array)) throw new TypeError("compare: target must be a Buffer or Uint8Array");
      sourceStart = sourceStart === undefined ? 0 : sourceStart | 0;
      sourceEnd = sourceEnd === undefined ? this.length : sourceEnd | 0;
      targetStart = targetStart === undefined ? 0 : targetStart | 0;
      targetEnd = targetEnd === undefined ? target.length : targetEnd | 0;
      const a = this.subarray(sourceStart, sourceEnd);
      const b = target.subarray(targetStart, targetEnd);
      return Buffer.compare(a, b);
    }

    indexOf(value, byteOffset, encoding) {
      if (typeof byteOffset === "string") { encoding = byteOffset; byteOffset = 0; }
      byteOffset = byteOffset | 0;
      if (byteOffset < 0) byteOffset = Math.max(0, this.length + byteOffset);
      let needle;
      if (typeof value === "number") {
        return Uint8Array.prototype.indexOf.call(this, value & 0xff, byteOffset);
      } else if (typeof value === "string") {
        needle = bytesFromString(value, encoding);
      } else if (value instanceof Uint8Array) {
        needle = value;
      } else {
        return -1;
      }
      if (needle.length === 0) return byteOffset;
      outer: for (let i = byteOffset; i <= this.length - needle.length; i++) {
        for (let j = 0; j < needle.length; j++) {
          if (this[i + j] !== needle[j]) continue outer;
        }
        return i;
      }
      return -1;
    }

    lastIndexOf(value, byteOffset, encoding) {
      if (typeof byteOffset === "string") { encoding = byteOffset; byteOffset = this.length - 1; }
      byteOffset = byteOffset === undefined ? this.length - 1 : (byteOffset | 0);
      if (byteOffset < 0) byteOffset = this.length + byteOffset;
      let needle;
      if (typeof value === "number") {
        return Uint8Array.prototype.lastIndexOf.call(this, value & 0xff, byteOffset);
      } else if (typeof value === "string") {
        needle = bytesFromString(value, encoding);
      } else if (value instanceof Uint8Array) {
        needle = value;
      } else {
        return -1;
      }
      if (needle.length === 0) return Math.min(byteOffset, this.length);
      const lastStart = Math.min(byteOffset, this.length - needle.length);
      outer: for (let i = lastStart; i >= 0; i--) {
        for (let j = 0; j < needle.length; j++) {
          if (this[i + j] !== needle[j]) continue outer;
        }
        return i;
      }
      return -1;
    }

    includes(value, byteOffset, encoding) {
      return this.indexOf(value, byteOffset, encoding) !== -1;
    }

    // Node's `inspect` and friends — minimal stub for util.inspect users.
    inspect() {
      const max = 50;
      const slice = this.subarray(0, Math.min(this.length, max));
      let hex = "";
      for (let i = 0; i < slice.length; i++) {
        if (i > 0) hex += " ";
        hex += HEX_TABLE[slice[i] >> 4] + HEX_TABLE[slice[i] & 0xf];
      }
      const more = this.length > max ? " ... " + (this.length - max) + " more bytes" : "";
      return `<Buffer ${hex}${more}>`;
    }

    // ----- Numeric read* / write* -------------------------------
    // Generated below via DataView to keep this readable. The methods
    // are on the prototype directly (not via getter forwarding) so
    // hot loops don't pay an extra property hop.
  }

  // ----- Symbol.toStringTag — Node uses "Uint8Array" via inheritance -

  // ----- Static `Buffer.poolSize` (sentinel; we don't pool) -----
  Buffer.poolSize = 8192;

  // ----- Numeric reader/writer generation ---------------------------------
  // Node's surface: read* / write* for {U,}Int{8,16,32}{LE,BE},
  // Float{32,64}{LE,BE}, Big{U,}Int64{LE,BE}, plus the variable-byte
  // read/writeIntLE/BE / readUIntLE/BE / writeUIntLE/BE (1..6 bytes).
  // All implemented via DataView for spec-correct endianness.
  const dvOf = (buf, off, len) =>
    new DataView(buf.buffer, buf.byteOffset + (off | 0), len);

  function defRW(name, size, le, kind) {
    const reader = "read" + name;
    const writer = "write" + name;
    Buffer.prototype[reader] = function (offset) {
      offset = offset | 0;
      if (offset < 0 || offset + size > this.length) {
        throw new RangeError(`${reader}: out of range`);
      }
      const dv = dvOf(this, offset, size);
      switch (kind) {
        case "u8": return dv.getUint8(0);
        case "i8": return dv.getInt8(0);
        case "u16": return dv.getUint16(0, le);
        case "i16": return dv.getInt16(0, le);
        case "u32": return dv.getUint32(0, le);
        case "i32": return dv.getInt32(0, le);
        case "f32": return dv.getFloat32(0, le);
        case "f64": return dv.getFloat64(0, le);
        case "bu64": return dv.getBigUint64(0, le);
        case "bi64": return dv.getBigInt64(0, le);
      }
    };
    Buffer.prototype[writer] = function (value, offset) {
      offset = offset | 0;
      if (offset < 0 || offset + size > this.length) {
        throw new RangeError(`${writer}: out of range`);
      }
      const dv = dvOf(this, offset, size);
      switch (kind) {
        case "u8": dv.setUint8(0, value & 0xff); break;
        case "i8": dv.setInt8(0, value); break;
        case "u16": dv.setUint16(0, value & 0xffff, le); break;
        case "i16": dv.setInt16(0, value, le); break;
        case "u32": dv.setUint32(0, value >>> 0, le); break;
        case "i32": dv.setInt32(0, value, le); break;
        case "f32": dv.setFloat32(0, value, le); break;
        case "f64": dv.setFloat64(0, value, le); break;
        case "bu64": dv.setBigUint64(0, BigInt(value), le); break;
        case "bi64": dv.setBigInt64(0, BigInt(value), le); break;
      }
      return offset + size;
    };
  }

  // 8-bit (no endian)
  defRW("UInt8", 1, true, "u8");
  defRW("Int8", 1, true, "i8");
  // 16/32-bit LE + BE
  defRW("UInt16LE", 2, true,  "u16");
  defRW("UInt16BE", 2, false, "u16");
  defRW("Int16LE",  2, true,  "i16");
  defRW("Int16BE",  2, false, "i16");
  defRW("UInt32LE", 4, true,  "u32");
  defRW("UInt32BE", 4, false, "u32");
  defRW("Int32LE",  4, true,  "i32");
  defRW("Int32BE",  4, false, "i32");
  // Float
  defRW("FloatLE",  4, true,  "f32");
  defRW("FloatBE",  4, false, "f32");
  defRW("DoubleLE", 8, true,  "f64");
  defRW("DoubleBE", 8, false, "f64");
  // BigInt 64
  defRW("BigUInt64LE", 8, true,  "bu64");
  defRW("BigUInt64BE", 8, false, "bu64");
  defRW("BigInt64LE",  8, true,  "bi64");
  defRW("BigInt64BE",  8, false, "bi64");

  // Variable-byte reads/writes (1..6 bytes). Node accepts byteLength
  // 1..6 and reads as signed/unsigned multi-byte integer.
  function readIntLEImpl(off, byteLen) {
    off = off | 0; byteLen = byteLen | 0;
    if (byteLen < 1 || byteLen > 6) throw new RangeError("byteLength must be 1..6");
    if (off < 0 || off + byteLen > this.length) throw new RangeError("readIntLE: out of range");
    let val = 0; let mul = 1;
    for (let i = 0; i < byteLen; i++) { val += this[off + i] * mul; mul *= 0x100; }
    // Sign-extend at byteLen bits.
    const bits = byteLen * 8;
    const lim = Math.pow(2, bits - 1);
    if (val >= lim) val -= Math.pow(2, bits);
    return val;
  }
  function readIntBEImpl(off, byteLen) {
    off = off | 0; byteLen = byteLen | 0;
    if (byteLen < 1 || byteLen > 6) throw new RangeError("byteLength must be 1..6");
    if (off < 0 || off + byteLen > this.length) throw new RangeError("readIntBE: out of range");
    let val = 0;
    for (let i = 0; i < byteLen; i++) val = val * 0x100 + this[off + i];
    const bits = byteLen * 8;
    const lim = Math.pow(2, bits - 1);
    if (val >= lim) val -= Math.pow(2, bits);
    return val;
  }
  function readUIntLEImpl(off, byteLen) {
    off = off | 0; byteLen = byteLen | 0;
    if (byteLen < 1 || byteLen > 6) throw new RangeError("byteLength must be 1..6");
    if (off < 0 || off + byteLen > this.length) throw new RangeError("readUIntLE: out of range");
    let val = 0; let mul = 1;
    for (let i = 0; i < byteLen; i++) { val += this[off + i] * mul; mul *= 0x100; }
    return val;
  }
  function readUIntBEImpl(off, byteLen) {
    off = off | 0; byteLen = byteLen | 0;
    if (byteLen < 1 || byteLen > 6) throw new RangeError("byteLength must be 1..6");
    if (off < 0 || off + byteLen > this.length) throw new RangeError("readUIntBE: out of range");
    let val = 0;
    for (let i = 0; i < byteLen; i++) val = val * 0x100 + this[off + i];
    return val;
  }
  function writeUIntLEImpl(value, off, byteLen) {
    off = off | 0; byteLen = byteLen | 0;
    if (byteLen < 1 || byteLen > 6) throw new RangeError("byteLength must be 1..6");
    if (off < 0 || off + byteLen > this.length) throw new RangeError("writeUIntLE: out of range");
    let v = Number(value);
    for (let i = 0; i < byteLen; i++) { this[off + i] = v & 0xff; v = Math.floor(v / 0x100); }
    return off + byteLen;
  }
  function writeUIntBEImpl(value, off, byteLen) {
    off = off | 0; byteLen = byteLen | 0;
    if (byteLen < 1 || byteLen > 6) throw new RangeError("byteLength must be 1..6");
    if (off < 0 || off + byteLen > this.length) throw new RangeError("writeUIntBE: out of range");
    let v = Number(value);
    for (let i = byteLen - 1; i >= 0; i--) { this[off + i] = v & 0xff; v = Math.floor(v / 0x100); }
    return off + byteLen;
  }
  function writeIntLEImpl(value, off, byteLen) {
    let v = Number(value);
    if (v < 0) v += Math.pow(2, byteLen * 8);
    return writeUIntLEImpl.call(this, v, off, byteLen);
  }
  function writeIntBEImpl(value, off, byteLen) {
    let v = Number(value);
    if (v < 0) v += Math.pow(2, byteLen * 8);
    return writeUIntBEImpl.call(this, v, off, byteLen);
  }
  Buffer.prototype.readIntLE = readIntLEImpl;
  Buffer.prototype.readIntBE = readIntBEImpl;
  Buffer.prototype.readUIntLE = readUIntLEImpl;
  Buffer.prototype.readUIntBE = readUIntBEImpl;
  Buffer.prototype.readUintLE = readUIntLEImpl; // legacy lower-cased alias
  Buffer.prototype.readUintBE = readUIntBEImpl;
  Buffer.prototype.writeIntLE = writeIntLEImpl;
  Buffer.prototype.writeIntBE = writeIntBEImpl;
  Buffer.prototype.writeUIntLE = writeUIntLEImpl;
  Buffer.prototype.writeUIntBE = writeUIntBEImpl;
  Buffer.prototype.writeUintLE = writeUIntLEImpl;
  Buffer.prototype.writeUintBE = writeUIntBEImpl;

  // -------------------------------------------------------------
  // Module-level helpers — Node 22 surface
  // -------------------------------------------------------------
  const kMaxLength = 0xffffffff; // 2 ** 32 - 1
  const constants = Object.freeze({ MAX_LENGTH: kMaxLength });

  function isUtf8Fn(input) {
    if (!ArrayBuffer.isView(input) && !(input instanceof ArrayBuffer)) {
      throw new TypeError("isUtf8: input must be a TypedArray, DataView, or ArrayBuffer");
    }
    try {
      new TextDecoder("utf-8", { fatal: true }).decode(input);
      return true;
    } catch { return false; }
  }
  function isAsciiFn(input) {
    if (!ArrayBuffer.isView(input) && !(input instanceof ArrayBuffer)) {
      throw new TypeError("isAscii: input must be a TypedArray, DataView, or ArrayBuffer");
    }
    const view = ArrayBuffer.isView(input)
      ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
      : new Uint8Array(input);
    for (let i = 0; i < view.length; i++) if (view[i] > 0x7f) return false;
    return true;
  }

  function makeCallableBuffer(BufferClass) {
    function NodeBuffer(arg, encodingOrOffset, length) {
      if (typeof arg === "number") return BufferClass.alloc(arg);
      return BufferClass.from(arg, encodingOrOffset, length);
    }
    Object.setPrototypeOf(NodeBuffer, BufferClass);
    Object.defineProperty(NodeBuffer, "prototype", {
      value: BufferClass.prototype,
      writable: false,
      enumerable: false,
      configurable: false,
    });
    Object.defineProperty(BufferClass.prototype, "constructor", {
      value: NodeBuffer,
      writable: true,
      enumerable: false,
      configurable: true,
    });
    return NodeBuffer;
  }

  const NodeBuffer = makeCallableBuffer(Buffer);

  return {
    Buffer: NodeBuffer,
    kMaxLength,
    INSPECT_MAX_BYTES: 50,
    constants,
    atob: globalThis.atob,
    btoa: globalThis.btoa,
    isUtf8: isUtf8Fn,
    isAscii: isAsciiFn,
  };
})();
"#;
