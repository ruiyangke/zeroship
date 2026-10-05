# Runtime limits

Every request an app serves runs inside a budget defined by its plan: a CPU
budget, a wall-clock timeout, and a JavaScript-heap cap. This page states the
built-in tiers, what each limit counts, and the error a caller sees when one
fires. Where a CPU, wall, or heap limit fires the platform answers a JSON error
of the form `{"message": "<text>", "name": "Error"}`; the request-size refusals
below use a different, shorter body.

## Effective limits per plan

A plan is the operator-maintained catalog entry that sets your limits; you do
not author it. You assign a plan and read back the one in force through the
billing API — see [Billing and metering](billing-metering.md). The built-in
tiers are `free`, `pro`, and `unlimited`; only `free` and `pro` are
creator-assignable, and `unlimited` is operator-only.

| Plan | CPU budget | Wall timeout | Heap cap |
| --- | --- | --- | --- |
| free | 50 ms | 5 s | 64 MB |
| pro | 30 s | 30 s | 256 MB |
| unlimited | none | none | none |

- An app with no plan, or whose plan cannot be read, runs at the **free** tier,
  never unbounded. Read the plan actually in force with
  `GET /api/apps/{id}/billing-status` before you size behavior to a higher tier.
- **unlimited** removes all three caps and is assigned by an operator, never by
  an app.
- The rows above are built-in defaults. The catalog is operator-maintained and
  per deployment, so treat these as what the platform ships, not as a value
  fixed for every deployment.

"None" means no runtime cap of that kind — not a zero-sized one.

The CPU budget deserves care before you design around it:

- **The CPU budget is CPU time, not wall time.** Time spent blocked — waiting on
  a database round trip, an object fetch, or a busy machine — does not count.
  What counts is the work your handler does with the bytes: parsing, copying,
  encoding. Waiting counts toward the wall timeout instead.
- **CPU time accumulates across the handler's lifetime.** It is not reset on an
  `await`; a handler that spends 10 ms of CPU after each of five awaits has used
  50 ms. The wall timeout likewise spans the whole request, waits included.
- There is no intermediate tier between 50 ms and 30 s. A handler that exceeds
  the free tier's budget needs the pro tier.

## CPU limit

A handler that exhausts its CPU budget is stopped, and the request fails with a
JSON error. For an `async` handler — the normal case — the platform answers
`500`:

```
500  {"message":"CPU time limit exceeded","name":"Error"}
```

A synchronous overrun — a handler that returns a response without ever
`await`ing — is answered `503` with the same envelope but the cause blanked:

```
503  {"message":"internal error","name":"Error","request_id":"<id>"}
```

There is no `code`, and the `name` is always `"Error"`. Treat "non-2xx" as the
contract and let the `message` text (when present) tell you the cause; a
`"message":"internal error"` at 5xx is the same kind of stop with the detail
removed.

## Pump CPU share, on every plan

The CPU budget above is charged in CPU time. A second limit polices the work
your app runs outside any single request: the timer callbacks and promise
continuations the isolate's event pump executes, which is where a streaming
loop's per-chunk step runs once a read or write has resolved. That pump work is
measured against wall time, and the check applies on **every** plan, including
`unlimited`. It is the one runtime cap `unlimited` does not remove.

The runtime adds up the JavaScript time your app spends in that pump work and
compares it against the wall time of a window:

| | value |
| --- | --- |
| window | 10 s |
| largest share of the window spent running pump work | 80% |

Windows are fixed, not rolling. The first one starts when the isolate is
created. Each time the pump finishes a batch of your callbacks, it checks
whether the current window has lasted at least 10 s. If it has, the share is
taken over the whole time since the window began: above 80%, the isolate is
stopped; otherwise that window ends and the next one starts. Time in which no
pump work runs still counts toward the window, so a window can last longer than
10 s and idle time lowers its share.

Not every callback is counted. Callbacks of zero-delay timers
(`setTimeout(fn, 0)`) can run outside the measured part of the pump, and those
are not counted toward the share.

An isolate whose pump-side JavaScript exceeds the share is stopped. The share
is measured per isolate, not per request, so the stop ends everything that
isolate was doing:

- Every request still waiting on it fails at once, including a request that
  was only waiting on something else and did none of the work:

  ```
  500  {"message":"CPU time limit exceeded","name":"Error"}
  ```

  The answer comes when the stop happens, on every plan. On `unlimited`, which
  has no wall timeout, nothing else would ever answer those requests.
- A response that is already streaming its body is cut off. The connection
  closes before the body's end, so a client reading the body sees the read
  fail rather than a body that ended normally.
- A streaming RPC procedure's stream ends in an error after the items it sent
  before the stop: called through `@zeroship/rpc`, the iteration throws an
  `RpcError` with code `INTERNAL` and message `CPU time limit exceeded`.
- None of your code runs in that isolate again. Its timers and pending
  operations are cancelled.

The next request to your deployed app is served by a fresh isolate, which runs
your module's top-level code again, so state held in module scope does not
survive the stop.

If the share is exceeded while your module is still starting, during a
top-level `await`, the app fails to load, and the request that started the load
answers

```
503  {"error":"failed to load app: failed to load bundle: failed to initialize app runtime: module init failed: CPU time limit exceeded"}
```

Every later request starts a fresh load, and fails the same way for as long as
startup exceeds the share.

The standalone dev server (`zeroship serve`) also replaces a stopped isolate.
Its next request is served by a fresh isolate that runs your module's top-level
code again, so state held in module scope does not survive the stop. This holds
whether the isolate stopped while starting or after it was serving. A stop
during startup is retried the same way: every later request starts a fresh load
and fails the same way for as long as startup exceeds the share.

A long CPU-bound loop over a stream is the shape that reaches this limit: each
per-chunk step is a promise continuation, so a fast storage path leaves little
waiting between steps and the JavaScript share of the window climbs toward the
whole of it. Yield in proportion to the work each step does: time the step's
synchronous section and, every so often, sleep for the time it measured. Time
spent waiting on storage is then never counted as compute.

## Wall timeout

The wall timeout bounds total elapsed time per request, however the handler
spends it — CPU, I/O, or waiting on a call it made. When it fires, the request
is answered with

```
504  {"message":"request timed out","name":"Error"}
```

### Dev applies no wall timeout and no CPU limit by default

The standalone dev server (`zeroship serve`, and therefore `pnpm dev`) applies
no wall timeout and no CPU limit unless you set them. A handler that runs longer
than your plan's wall timeout therefore works locally and fails in production
with the `504` above, and dev gives no warning. Wall time is not the only limit
dev relaxes: its CPU limit is off by default and its heap default is above both
paid tiers (see "Heap limit"). The single limit dev applies *stricter* than
deployed is request-body size (below).

You can reproduce the deployed behavior for wall time and CPU locally — the dev
server accepts the same budgets as flags (both in milliseconds):

```bash
zeroship serve app.js --wall-timeout=5000
zeroship serve app.js --cpu-limit=50
```

Neither flag is on by default, and `pnpm dev` does not pass them. If any request
can run long, exercise it once with your plan's `--wall-timeout` before you
ship.

## Request body size

### Deployed

An inbound request body is capped at **4 MiB** per request. It is a
platform-wide limit, not a per-app knob. A body beyond it is refused with
**`400`** and a plain-text, non-JSON body — not a `413`, and not a JSON error.
The body does not name the limit, so do not match on its text; match on the
`400` and on the body not being your app's JSON. The cap applies before your
handler runs.

Large uploads do not belong in the request body: send them to object storage
with `bucket(name).put(key, value)` from `@zeroship/storage` (see
[Object storage](storage.md)). The object bytes stream to storage instead of
being buffered in memory, and they never pass through this cap.

**A `413` on this path means a different limit fired.** A resource — an entry in
your `defineApp` config (a URL path or an RPC procedure) — can declare its own
`maxInputBytes` cap, in bytes. That cap answers `413`:

| what fired | status | body |
| --- | --- | --- |
| the 4 MiB request cap | `400` | plain text, no JSON |
| a per-resource `maxInputBytes` cap | `413` | `{"error":"input exceeds max_input_bytes"}` |
| your app rejecting the payload | `400` | your app's JSON |

You set `maxInputBytes` on a resource entry
(`resources: { "<path-or-rpc:…>": { maxInputBytes } }`), as an RPC default
(`rpc: { defaults: { maxInputBytes } }`), or on a procedure's own config. Note
the first and third rows share a status: a `400` alone does not tell you whether
the platform refused the bytes or your handler did.

### `zeroship serve` caps bodies at 1 MiB, not 4

The standalone dev server has its own, smaller cap: **1 MiB**, answered as
`413 Content Too Large` (empty body). The two tiers therefore disagree by 4x,
and dev is the stricter one:

| tier | request body cap |
| --- | --- |
| `pnpm dev` / `zeroship serve` | 1 MiB |
| deployed | 4 MiB |

The dev boundary is exact, and a body of exactly 1 MiB is accepted — the check
is "greater than", not "greater than or equal":

| body bytes | response |
| --- | --- |
| 1 048 575 | `200` |
| 1 048 576 | `200` |
| 1 048 577 | `413` |
| 2 097 152 | `413` |

This fails in the safe direction: a body your app accepts locally is also
accepted in production. The trap is the reverse reading — a 2 MiB request that
answers `413` under `pnpm dev` is not evidence that the platform rejects it:
deployed accepts up to 4 MiB. The dev body cap has no flag, so the
dev-vs-deployed body-size difference is the one you cannot reproduce locally.
Route large payloads through object storage regardless.

## Heap limit

The heap cap bounds the JavaScript heap — the in-memory objects and strings your
handler holds. It is not a cap on your bundle size or on request/response body
bytes. On the built-in tiers, free apps are capped at 64 MB and pro at 256 MB;
the unlimited plan has no cap.

A handler that keeps allocating past the cap is stopped, and the request fails
with a JSON error. For an `async` handler the platform answers `500`:

```
500  {"message":"memory limit exceeded","name":"Error"}
```

A synchronous overrun is answered `503` with the cause blanked
(`{"message":"internal error","name":"Error","request_id":"<id>"}`). As with the
CPU limit, there is no `code`, the `name` is always `"Error"`, and "non-2xx" is
the contract.

The dev server's heap default is 512 MB (`zeroship serve --heap-limit-mb`, or
the `ZEROSHIP_HEAP_LIMIT_MB` environment variable), so a bundle that loads large
dependencies can run locally and still exceed the free tier's 64 MB in
production. Size the plan for what you import.