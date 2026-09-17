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