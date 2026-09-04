# shellcheck shell=bash
# ============================================================================
# tests/lib/dispatch_frame.sh — build the binary frame the worker's /dispatch
# endpoint actually decodes.
#
# The wire format is crates/zeroship-core/src/dispatch_frame.rs `encode_dispatch_frame`:
#
#     [ 4 bytes: metadata length, little-endian ]
#     [ metadata JSON: {method, url, headers}   ]
#     [ raw body bytes                          ]
#
# The body is NOT a field of the metadata. That distinction is the whole
# reason this file exists: the harnesses used to POST a single JSON object
# with the body inline, and `decode_dispatch_frame` read its first four
# bytes — `{"me` — as the length prefix. That is 1,701,651,067 against a
# 64 KiB cap, so every request was refused with
# `invalid envelope: dispatch metadata too large` before reaching the
# runtime. Whole stages appeared to test a primitive while no request ever
# got in.
#
# It lives here because three harnesses need it and the format is versioned
# code, not test scaffolding. A copy per harness is how the previous shape
# survived a wire-format change in the first place.
#
#   zs_write_frame <outfile> <method> <url> <body>
#   zs_rpc_frame   <outfile> <procedure-id> <args-json>
#
# `zs_rpc_frame` is the common case: it targets /__zeroship/v1/<id> and wraps
# the args as {"json": <args>}, matching how the dispatcher unwraps input.
# ============================================================================

#   zs_write_frame <outfile> <method> <url> <body> [headers-json]
#
# headers-json is an optional [["name","value"],...] array; it defaults to a
# single content-type: application/json. Pass it when a procedure needs more,
# e.g. `accept: text/event-stream` for a streaming RPC — those headers belong
# in the frame metadata, not on the outer POST to /dispatch.
zs_write_frame() {
    node -e '
const fs = require("node:fs");
const [out, method, url, body, headersJson] = process.argv.slice(1);
const meta = Buffer.from(JSON.stringify({
  method,
  url,
  headers: headersJson
    ? JSON.parse(headersJson)
    : [["content-type", "application/json"]],
}), "utf8");
const len = Buffer.alloc(4);
len.writeUInt32LE(meta.length, 0);
fs.writeFileSync(out, Buffer.concat([len, meta, Buffer.from(body, "utf8")]));
' "$@"
}

zs_rpc_frame() {
    local out="$1" id="$2" args="$3"
    local body
    body="$(node -e 'process.stdout.write(JSON.stringify({json:JSON.parse(process.argv[1])}))' "$args")"
    zs_write_frame "$out" POST "http://x/__zeroship/v1/$id" "$body"
}
