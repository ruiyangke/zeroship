# stream-probe

A deliberately tiny app whose only job is to be driven over the wire by
`tests/`: it emits a paced SSE stream with no external dependency.

`examples/ai-chat` is the only other app with a `stream` procedure, and it calls
a paid API. Measuring chunk delivery through it would mostly measure that
provider's latency, and could not run at all without spend.

## What it exposes

- `probe.ticks` (stream) - 5 chunks, 200ms apart, each `{ mark, i, of, atMs }`.
- `probe.ping` (query) - proves the app is reachable before anything reasons
  about timing, so a connection failure and a buffered stream are not confused.

## Why the emission is paced

A proxy that buffers a response still delivers every chunk, in order, with
identical bytes. A test comparing only the assembled result would pass while
streaming was entirely broken. Pacing makes time-to-first-chunk the
discriminator: streaming delivers chunk 1 at ~200ms and chunk 5 at ~1000ms;
buffering delivers all five at ~1000ms. Measured in dev:

```
+ 214ms  {"mark":"TICK","i":1,"of":5,...}
+ 414ms  {"mark":"TICK","i":2,"of":5,...}
+ 615ms  {"mark":"TICK","i":3,"of":5,...}
+ 815ms  {"mark":"TICK","i":4,"of":5,...}
+1015ms  {"mark":"TICK","i":5,"of":5,...}
```

## Two things this app is shaped by

`src/index.ts` starts with `"use server";`. Without it the build succeeds,
prints `0 server functions`, and still emits a manifest declaring both RPC
resources - so every procedure 404s at runtime with no build error.

`src/server/config.ts` opts both procedures into anonymous access. Without it
they resolve to `auth: "user"` and the gateway refuses them, which is green in
`pnpm dev` and 401 deployed.
