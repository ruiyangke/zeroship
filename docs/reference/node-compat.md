# Node.js Compatibility

Zeroship does not run Node.js. Your app code runs in a V8 isolate whose real
surface is the Web platform — `fetch`, `Request`/`Response`, `ReadableStream`,
`Blob`, `TextEncoder`, `crypto.subtle`. Reach for a `node:` import only for the
modules listed on this page; everything else is a polyfill or absent.

Two layers decide what a `node:` import gives you:

- The **runtime** implements a fixed set of specifiers natively, with no polyfill.
- The **build** either leaves an import for the runtime to resolve, or replaces it
  with a polyfill before the runtime ever sees it.

Both are described below, so you can tell what actually runs in a deployed app.

## Runtime-native `node:` modules

The runtime resolves these specifiers itself, natively:

`node:async_hooks`, `node:buffer`, `node:crypto`, `node:events`, `node:net`,
`node:os`, `node:path`, `node:tls`, `node:util`, `node:zlib`.

The build currently keeps only `node:async_hooks`, `node:buffer`, `node:crypto`,
`node:path` and `node:util` bare. The other five (`events`, `net`, `os`, `tls`,
`zlib`) are replaced by a polyfill first, so a built app does not reach the
native implementations described for them here — see
[Build-time compatibility](#build-time-compatibility) for what that means.

A static import of a `node:` specifier the runtime does not resolve fails module
resolution, naming the specifier and the importing module. Dynamic `import()`
rejects with `Cannot find module '<specifier>'`.

### `node:async_hooks`

`AsyncLocalStorage` is native and is the only supported export. It propagates a
store across `await`, microtasks, `.then`, generator yields and native-Promise
resolution.

- `run(store, fn, ...args)`, `getStore()`, `enterWith(store)`, `disable()`.
- `AsyncResource`, `createHook`, `executionAsyncId`, `triggerAsyncId` and
  `executionAsyncResource` exist as functions that throw an `Error` coded
  `ERR_METHOD_NOT_IMPLEMENTED` when called; `asyncWrapProviders` is an empty
  object. `new AsyncResource(...)` throws the same way.

There is no `exit()`.

### `node:buffer`

`Buffer` is a real class extending `Uint8Array`, and it is also
`globalThis.Buffer` — the global and the module export are the same constructor.
`Buffer(n)` is callable and behaves like `Buffer.alloc(n)`.

- Encodings: `utf8`/`utf-8`, `utf16le`/`utf-16le`/`ucs2`/`ucs-2`,
  `latin1`/`binary`, `ascii`, `hex`, `base64`, `base64url`. An unknown encoding
  throws `TypeError: Unknown encoding: <name>`.
- Statics: `alloc`, `allocUnsafe`, `allocUnsafeSlow`, `from`, `of`,
  `byteLength`, `compare`, `concat`, `isBuffer`, `isEncoding`, `copyBytesFrom`.
  `allocUnsafe` still returns zero-filled memory.
- Instances: `toString`, `toJSON`, `write`, `fill`, `copy`, `subarray`, `slice`,
  `equals`, `compare`, `indexOf`, `lastIndexOf`, `includes`, `inspect`, and the
  full numeric `read*`/`write*` set (fixed-width `UInt`/`Int`/`Float`/`Double`/
  `BigInt64`, little- and big-endian, plus the variable-length byte readers).
  `subarray` and `slice` return a `Buffer`, not a plain `Uint8Array`.
- `kMaxLength` and `constants.MAX_LENGTH` are `0xffffffff`; `INSPECT_MAX_BYTES`
  is `50`; `constants.MAX_STRING_LENGTH` is undefined. `isUtf8`, `isAscii`,
  `atob` and `btoa` are exported.
- Absent: `Buffer.transcode`, `resolveObjectURL`, `transferAsUncopied`.

### `node:crypto`

Hashing, HMAC, random, KDFs, key objects, sign/verify, ciphers and key generation
are implemented. `webcrypto` and `subtle` are the same objects as
`globalThis.crypto` and `globalThis.crypto.subtle`.

- Hashing: `createHash`, `createHmac`; `update`, `digest`, `copy` (on `Hash`).
  A second `digest` or an `update` after `digest` throws
  `ERR_CRYPTO_HASH_FINALIZED`.
- Random: `randomBytes` (returns a `Buffer`, or fires a callback when one is
  passed), `randomFillSync`, `randomFill`, `randomInt`, `randomUUID`,
  `getRandomValues`.
- Key derivation: `pbkdf2Sync`/`pbkdf2`, `hkdfSync`/`hkdf`,
  `scryptSync`/`scrypt`. The async forms compute synchronously and resolve
  through the microtask queue, so they do not yield to I/O.
- Keys: `createSecretKey`, `createPublicKey`, `createPrivateKey`, `KeyObject`
  (with `KeyObject.from`), `PublicKeyObject`, `PrivateKeyObject`,
  `SecretKeyObject`, from PEM, DER or JWK input. Importing or exporting an
  encrypted PKCS#8 key throws `ERR_CRYPTO_UNSUPPORTED_OPERATION`.
- Sign / verify: `createSign`, `createVerify`, `sign`, `verify`, plus
  `publicEncrypt` and `privateDecrypt` (RSA-OAEP).
- Ciphers: `createCipheriv`, `createDecipheriv`, `getCipherInfo`; AES-CBC,
  AES-CTR, AES-GCM and ChaCha20-Poly1305. `createCipher` and `createDecipher`
  (the deprecated password form) throw `ERR_CRYPTO_UNSUPPORTED_OPERATION`.
  `authTagLength` throws `ERR_INVALID_ARG_VALUE`.
- Key generation: `generateKeySync` (`hmac`, `aes`), `generateKeyPairSync`
  (`rsa`, `ec`, `ed25519`, `x25519`). No async `generateKeyPair` or
  `generateKey`.
- Misc: `timingSafeEqual` (unequal lengths throw
  `ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH`), `getHashes`, `getCurves`, `getFips`
  (`0`), `setFips` (`setFips(true)` throws `ERR_CRYPTO_OPERATION_FAILED`),
  `secureHeapUsed` (all zeros), `fips` (`0`) and `constants` (RSA padding
  values).
- `getCiphers()` returns an **empty** array even though the cipher classes work;
  do not use it to feature-detect. `getCipherInfo(name)` returns `undefined` for
  an unknown name.

### `node:events`

`EventEmitter` is implemented with the usual `on`/`addListener`,
`prependListener`, `once`, `prependOnceListener`, `off`/`removeListener`,
`removeAllListeners`, `emit`, `listeners`, `rawListeners`, `eventNames`,
`listenerCount`, `setMaxListeners`, `getMaxListeners`, plus `newListener` and
`removeListener` notifications and `captureRejections`. `errorMonitor` and
`captureRejectionSymbol` are exported, and the module default is the same
namespace.

`errorMonitor` listeners fire before an `error` event; an `error` event with no
listener throws, as in Node.

### `node:net`

Raw TCP, gated. **An app with no egress rules cannot import `node:net` or
`node:tls` at all** — the specifier does not resolve. Any rule (accept or reject)
makes the specifier resolvable, and each connection is then checked against your
rules. Rules are a non-ordered set: a matching reject wins, otherwise a matching
accept admits, otherwise the connection is refused.

Raw-socket calls are only allowed from `action`, `stream` and `subscription`
handlers. A `query` or `mutation` handler that connects fails with a
`capability_violation` error.

- Exports: `Socket`, `createConnection`, `connect`, `isIP`, `isIPv4`, `isIPv6`.
- `Socket` supports `connect`, `write`, `end`, `destroy`, `pause`, `resume`,
  `setNoDelay`, `setKeepAlive`, `setTimeout`, `cork`/`uncork`, `ref`/`unref`,
  `setEncoding`; the read-only `remoteAddress`, `remotePort`, `bytesRead`,
  `bytesWritten` and `encrypted`; and the `connect`, `ready`, `data`, `drain`,
  `end`, `error` and `close` events.
- A `path` option (Unix domain sockets) throws `ERR_NOT_IMPLEMENTED`. A port
  outside `1`–`65535` throws a `RangeError`. Writes issued before the socket
  connects are buffered; the buffer is capped, and exceeding it destroys the
  socket with `ERR_NET_WRITE_CAP`.
- Connection-level failures arrive on the `error` event: `ERR_NET_EGRESS_DENIED`
  when your rules refuse the target, `ERR_NET_SSRF` when the platform's address
  floor refuses it, `ERR_NET_DNS`/`ERR_NET_DNS_TIMEOUT` on resolution failure,
  and `ERR_NET_EGRESS_CAP` when the connection's byte ceiling is reached (the
  socket is then destroyed). Concurrency and total bytes are capped by your plan.

The runtime's native `node:net` and `node:tls` are described by these facts, but
the build currently replaces their imports with a polyfill whose connect path is
a stub (see below).

### `node:os`

Values are constants chosen for a single-tenant Linux isolate, not real host
introspection. `platform()` is `linux`, `arch()`/`machine()` map the build
target, `type()` is `Linux`, `release()` is `6.0.0`, `version()` is a fixed
string, `hostname()` is `zeroship-worker`, `endianness()` is `LE`, `EOL` is
`\n`. `cpus()` returns one synthetic CPU, `availableParallelism()` is `1`,
`totalmem()` is 1 GiB, `freemem()` is 512 MiB, `loadavg()` is `[0, 0, 0]`,
`uptime()` counts seconds since the module was imported, and `userInfo()`
reports `username: "zeroship"` with `uid`/`gid` `-1` and a `null` shell.
`homedir()` is `/`, `tmpdir()` is `/tmp`.

`networkInterfaces()` returns an empty object; `getPriority()` returns `0`;
`setPriority()` is a no-op. None of them throw. `constants.signals` and
`constants.priority` mirror the usual tables.

### `node:path`

POSIX only. `sep` is `/`, `delimiter` is `:`, and `posix` is the module itself.
Implemented: `resolve`, `normalize`, `isAbsolute`, `join`, `relative`, `dirname`,
`basename`, `extname`, `format`, `parse`.

`win32` methods throw `ERR_METHOD_NOT_IMPLEMENTED`; only `win32.sep` (`\`) and
`win32.delimiter` (`;`) are readable. There are no drive letters or backslash
semantics.

### `node:tls`

TLS over TCP, gated exactly like `node:net` (same egress rules and the same
`action`/`stream`/`subscription` restriction). Exports `TLSSocket`, `connect`
and `createSecureContext`.

- Options: `host`/`hostname`, `port`, `servername`, `rejectUnauthorized`
  (default `true`), `ca` (PEM string, array of PEM strings, or bytes), and
  `socket` to upgrade an existing connection.
- `rejectUnauthorized: false` is refused outside the local dev runtime with
  `ERR_TLS_REJECT_UNAUTHORIZED_DISABLED`.
- `checkServerIdentity` is **not invoked**. Supplying it switches to
  chain-only verification — the hostname is no longer checked — and requires a
  pinned `ca`; without one the connection fails with `ERR_TLS_PIN_REQUIRED`.
- Client certificate options (`cert`, `key`, `passphrase`, `pfx`) throw
  `ERR_NOT_IMPLEMENTED`. A `ca` that is not PEM throws `ERR_TLS_CA_INVALID`.
- Events are the `node:net` set plus `secureConnect`, which replaces `connect`
  as the "usable now" signal for a TLS socket.

### `node:util`

`format`, `formatWithOptions`, `inspect`, `inherits`, `promisify`,
`callbackify`, `deprecate`, the `types.*` predicates, `isDeepStrictEqual`,
`parseArgs`, `TextEncoder` and `TextDecoder` (the same classes as the globals).

- `inspect` handles primitives, arrays, plain objects, `Map`, `Set`, `Date`,
  `RegExp`, `Error` and functions, with circular detection and depth/array
  limits. `inspect.colors` and `inspect.styles` are empty objects;
  `inspect.custom` is present.
- `promisify` honours `util.promisify.custom`; `callbackify` requires a trailing
  callback.
- `parseArgs` supports long options (`--flag`, `--flag=value`, `--flag value`),
  `string`/`boolean` types and `multiple`. Short flags, `tokens` and
  `allowPositionals` are not implemented.

Absent: `styleText`, `MIMEType`, `MIMEParams`, `getSystemErrorName`,
`debuglog`/`debug`, and `aborted`.

### `node:zlib`

`gzip`/`gunzip`, `deflate`/`inflate`, `deflateRaw`/`inflateRaw` and
`brotliCompress`/`brotliDecompress`, each in a sync form and a callback form.
The callback forms compute synchronously and fire the callback through the
microtask queue. A bad argument type throws `ERR_INVALID_ARG_TYPE`; input that is
not valid for the codec throws an error coded `Z_DATA_ERROR`.

`createGzip`, `createGunzip`, `createDeflate`, `createInflate`,
`createDeflateRaw`, `createInflateRaw`, `createBrotliCompress` and
`createBrotliDecompress` throw `ERR_METHOD_NOT_IMPLEMENTED` — there are no Node
stream constructors. Use the sync/callback form, or `CompressionStream` /
`DecompressionStream` from the Web stream API.

`constants` carries the common flush values, return codes, compression levels,
strategies and Brotli parameters.

## Build-time compatibility

Every `node:` import is classified at build time:

- **Left bare** — `node:async_hooks`, `node:buffer`, `node:crypto`, `node:path`,
  `node:util`. The runtime resolves these natively.
- **Purpose-built replacement** — `node:timers/promises`, `node:module`,
  `node:process`.
- **General polyfill, over the runtime's own native implementation** —
  `node:events`, `node:net`, `node:os`, `node:tls`, `node:zlib`.
- **General polyfill** — everything else, when one exists.

The general polyfill is what makes imports like `node:url`, `node:querystring`
and `node:string_decoder` work. A function it does not implement throws an
`Error` whose message names the function and ends `is not implemented yet!`, at
call time — the import itself can succeed. That is the failure you see for most
of the Node standard library (`node:fs`, `node:http`, `node:worker_threads`,
`node:child_process`, and so on). Do not import them; use `fetch`, the Web
streams API, and the modules above.

The purpose-built replacements are:

- **`node:timers/promises`** — `setTimeout(ms, value)`, `setImmediate(value)` and
  `setInterval(ms, value)`, where `setInterval` is an async generator you can
  `for await` over. There is no `scheduler` object and no `AbortSignal`
  argument.
- **`node:module`** — `createRequire()` returns a callable no-op; `builtinModules`
  is empty, `isBuiltin` returns `false`, and `syncBuiltinESMExports` does
  nothing. There is no working CommonJS `require`.
- **`node:process`** — re-exports the runtime's `process` global, so bare
  `process`, the module default and `globalThis.process` are one object, and adds
  named fallbacks so code that destructures `cwd`, `chdir`, `exit`, `nextTick`,
  `hrtime` or the stdio/EventEmitter shims does not crash.

The `process` global itself carries `env`, `version` (`20.0.0`), `versions`
(`node`/`v8`/`openssl`), `platform` (`linux`), `arch` (`x64`), `hrtime` (with
`bigint`), `stdout`/`stderr` (whose writes are accepted and discarded) and
`nextTick`. The named fallbacks supply `cwd()` (returns `/`), a no-op `chdir()`
and `exit()`, and a no-op `EventEmitter` surface.

For the five replaced specifiers, a built app gets the polyfill, not the native
runtime surface: `node:events` and `node:zlib` are working replacements (their
top-level `setMaxListeners`/`listenerCount` helpers throw the not-implemented
error), `node:os` returns stub values, and `node:net`/`node:tls` are
non-functional stubs — `net.connect` and `net.createConnection` throw the
not-implemented error, and no stub socket ever opens a connection.

## Practical boundary

- **Write to the Web platform first.** `fetch`, `Request`/`Response`,
  `ReadableStream`, `Blob`, `TextEncoder`/`TextDecoder`, `crypto.subtle`,
  `WebSocket`, `AbortController` and `URL` are the runtime's native surface and
  are not compatibility shims.
- **Import a `node:` module only from the lists above.** Anything else is a
  polyfill that may throw when called, and `node:fs`-style modules are absent by
  design — there is no filesystem, no child processes and no worker threads.
- **`node:net`/`node:tls` are for outbound raw streams, not servers.** There is
  no `createServer`, they require egress rules, and they are callable only from
  `action`/`stream`/`subscription` handlers.
- **Failure is explicit.** An unimplemented call throws an `Error` naming the
  function; a missing module fails resolution (static import) or rejects
  `Cannot find module '<specifier>'` (dynamic import); a denied connection
  emits an `error` event with a code.