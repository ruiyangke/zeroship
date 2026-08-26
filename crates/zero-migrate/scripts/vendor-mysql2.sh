#!/usr/bin/env bash
set -euo pipefail

# Regenerate the committed mysql2 platform-driver bundle.
#
# Pinning:
#   mysql2:  3.14.1
#   esbuild: 0.28.0
#
# Runtime contract:
#   - `node:*` imports stay external and bind to the runtime host's native
#     SyntheticModules (`node:net`, `node:tls`, `node:buffer`, ...).
#   - Bare ambient Node modules that mysql2 references during bundling are
#     shimmed here; the mysql2 package source is not patched.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
OUT="${CRATE_DIR}/src/frontend/vendor/mysql2-3.14.1.bundle.mjs"

TMP="$(mktemp -d)"
cleanup() {
  rm -rf "${TMP}"
}
trap cleanup EXIT

cd "${TMP}"
cat > package.json <<'JSON'
{
  "private": true,
  "type": "module",
  "dependencies": {
    "esbuild": "0.28.0",
    "mysql2": "3.14.1"
  }
}
JSON

pnpm install --frozen-lockfile=false --lockfile-only=false

mkdir -p shims

cat > entry.mjs <<'JS'
import mysql from "mysql2/promise";
export default mysql;
JS

cat > shims/events.js <<'JS'
export * from "node:events";
import events from "node:events";
export default events;
JS

cat > shims/net.js <<'JS'
export * from "node:net";
import net from "node:net";
export default net;
JS

cat > shims/tls.js <<'JS'
export * from "node:tls";
export { createSecureContext } from "node:tls";
import tls from "node:tls";
export default tls;
JS

cat > shims/buffer.js <<'JS'
export * from "node:buffer";
import buffer from "node:buffer";
export default buffer;
JS

cat > shims/crypto.js <<'JS'
export * from "node:crypto";
export { constants, publicEncrypt } from "node:crypto";
import crypto from "node:crypto";
export default crypto;
JS

cat > shims/zlib.js <<'JS'
export * from "node:zlib";
import zlib from "node:zlib";
export default zlib;
JS

cat > shims/util.js <<'JS'
export * from "node:util";
import util from "node:util";
export default util;
JS

cat > shims/process.js <<'JS'
const fallback = {
  env: {},
  versions: { node: "22.0.0" },
  platform: "linux",
  arch: "x64",
  nextTick: (fn, ...args) => queueMicrotask(() => fn(...args)),
  hrtime: () => [0, 0],
  uptime: () => 0,
  binding(name) {
    if (name === "buffer") {
      return { kStringMaxLength: 0x1fffffe8 };
    }
    throw new Error(`process.binding(${name}) is not available`);
  }
};
const processObject = globalThis.process || fallback;
export const env = processObject.env;
export const versions = processObject.versions;
export const platform = processObject.platform;
export const arch = processObject.arch;
export const nextTick = processObject.nextTick.bind(processObject);
export default processObject;
JS

cat > shims/timers.js <<'JS'
export const setTimeout = globalThis.setTimeout.bind(globalThis);
export const clearTimeout = globalThis.clearTimeout.bind(globalThis);
export const setInterval = globalThis.setInterval.bind(globalThis);
export const clearInterval = globalThis.clearInterval.bind(globalThis);
export default { setTimeout, clearTimeout, setInterval, clearInterval };
JS

cat > shims/stream.js <<'JS'
import { EventEmitter } from "node:events";
export class Stream extends EventEmitter {}
export class Readable extends Stream {}
export class Writable extends Stream {}
export class Duplex extends Stream {}
export class Transform extends Duplex {}
export class PassThrough extends Transform {}
export default { Stream, Readable, Writable, Duplex, Transform, PassThrough };
JS

cat > shims/string_decoder.js <<'JS'
export class StringDecoder {
  write(value) {
    return typeof value === "string" ? value : String(value ?? "");
  }
  end(value = "") {
    return this.write(value);
  }
}
export default { StringDecoder };
JS

cat > shims/url.js <<'JS'
export const URL = globalThis.URL;
export const URLSearchParams = globalThis.URLSearchParams;
export default { URL, URLSearchParams };
JS

pnpm exec esbuild entry.mjs \
  --bundle \
  --format=esm \
  --platform=neutral \
  --main-fields=module,main \
  --outfile="${OUT}" \
  --external:node:* \
  --alias:events=./shims/events.js \
  --alias:net=./shims/net.js \
  --alias:tls=./shims/tls.js \
  --alias:buffer=./shims/buffer.js \
  --alias:crypto=./shims/crypto.js \
  --alias:zlib=./shims/zlib.js \
  --alias:util=./shims/util.js \
  --alias:process=./shims/process.js \
  --alias:timers=./shims/timers.js \
  --alias:stream=./shims/stream.js \
  --alias:string_decoder=./shims/string_decoder.js \
  --alias:url=./shims/url.js

echo "wrote ${OUT}"
