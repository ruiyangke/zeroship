# auth-uploads-kv

An authenticated file drop. The example that exercises **`env.auth` +
`env.storage` + `env.kv` + RPC together**, because every other example in
`examples/` exercises exactly one primitive and the interesting bugs live at
the seams between two.

```
src/index.ts       the five RPC procedures
scripts/smoke.sh   two-user curl harness - signs in as Alice AND Bob
vite.config.ts     dev auth with two users; ports 5183 (vite) / 3041 (runtime)
```

It deliberately does **not** use `env.db`. Storage and KV need no schema, so
this app actually runs under `pnpm dev` today; a migration-first `env.db` app
does not (see `examples/auth-notes-db` and its README).

## Procedures

| id               | kind     | behaviour |
| ---------------- | -------- | --------- |
| `files.upload`   | mutation | Reserves a KV rate-limit slot, then writes bytes under `u/<caller>/...`. The client never names a namespace. |
| `files.list`     | query    | `listAll("u/<caller>/")` - the prefix *is* the query. |
| `files.download` | query    | By key, but the key is re-checked against the caller's prefix first. |
| `files.delete`   | mutation | Same by-key scoping. Does **not** refund a slot. |
| `files.quota`    | query    | The caller's usage in the current window. |

Three seams are what this app is for:

**1. Identity decides the key.** `prefixFor(userId)` is the only place
ownership is expressed, and `assertOwned` re-derives it before any by-key
read or delete. Prefix-scoping `files.list` alone is *not* enough: a by-key
read that trusts the key it was handed serves any object in the bucket to
anyone who can name one, and user ids are not secrets. The refusal is a
**404**, not a 403 - "that object exists but is not yours" is itself a
disclosure.

**2. KV gates the write, before the write.** `reserveUploadSlot` is a single
atomic `incr` with `ttlMs` (applied only on key creation - the fixed-window
shape). A limit checked *after* the work is not a limit.

**3. Nothing is transactional across KV and storage.** If the reserve commits
and the write then throws, `releaseUploadSlot` is the only thing that puts the
counter back - the platform will not do it. That compensation is verified by
the smoke on two different failures (malformed body, oversize body). See the
caveat in `releaseUploadSlot`'s doc comment: the `has` guard against
resurrecting an expired window is not atomic with the decrement, and KV
exposes no compare-and-decrement to make it so. The failure is one-sided in
the safe direction.

## Run it

```bash
pnpm dev                 # vite on :5183 + the dev runtime on :3041
pnpm smoke               # the two-user harness (ZEROSHIP_URL to retarget)
pnpm build               # -> dist/app.zship
pnpm typecheck
```

`vite.config.ts` configures **two** dev users (`alice@localhost` / `alice`,
`bob@localhost` / `bob`). Two is the whole point: "A cannot reach B's object"
is not a claim you can test with one identity.

The smoke is re-runnable, but the app rate-limits *it* too, so a back-to-back
run may pause up to 60s at step `[0b]` waiting for the window to reset. There
is deliberately no "clear the limiter" procedure - that would be app surface
existing only for the test.

## Measured limits (not guessed)

`MAX_UPLOAD_BYTES` is 512 KiB because the runtime's HTTP server caps a request
body at 1 MiB (`MAX_BODY_BYTES`, `crates/runtime/src/core/serve.rs`) and base64
inflates by 4/3. The buffered `env.storage.put` would allow 16 MiB, but that
ceiling is unreachable over the JSON RPC wire - a limit the transport eats
before the handler sees it is not a limit the app can be said to enforce.
Genuinely large objects belong on `putStream`, which never materialises the
body in a request at all.

Note that a request just over the 1 MiB body cap gets a clean **413** when sent
straight to the runtime port, but through vite's dev proxy the same request can
come back as a bare `Connection reset by peer` with no HTTP status.

## What the smoke proves

Run it and read the output; every check prints the request and the response.
The checks that matter are the negatives, and each has a positive control next
to it so a refusal cannot pass vacuously:

- Bob's list *contains bob's own key* (control) and *does not contain Alice's*.
- Bob downloading and deleting **Alice's key** are both 404, and Alice can
  still download the object afterwards (proving the refusal was not a silent
  success).
- An upload is genuinely **refused with 429**, not merely counted, and the
  object count grows by exactly the number of accepted uploads.
- Alice is unaffected while Bob is blocked - the limit is per user, not global.
- After a post-reserve failure, `files.quota` is exactly where it started.

`accepted` can legitimately exceed the limit of 5: the window is a *fixed*
window, so a burst straddling a boundary gets a fresh allowance. That is the
documented behaviour of this limiter shape, not a leak.
