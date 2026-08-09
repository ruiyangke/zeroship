# storage-probe

A deliberately small app whose only job is to be driven over the wire by
`tests/e2e_dev_vs_deployed_storage.sh`: it exercises `@zeroship/storage` and
needs no login, no database, and no external service.

## Why it exists

Scenario 5 of `docs/pilot/e2e-scenarios.md` (`env.storage`) was walked in dev
only. The blocker was not storage: the only storage fixture,
`examples/auth-uploads-kv`, signs in through the dev-auth provider, which is
dev-only by construction, so its deployed side could not be driven at all.

Storage has no auth dependency. This app removes the coupling rather than
working around it -- no users, no per-user data, every key under one shared
`sp/` prefix -- so the same operations can run against `pnpm dev` and against
the deployed app behind the gateway and the results can be diffed.

## What it exposes

All fourteen procedures take no arguments and return plain JSON.

| procedure | what it pins down |
| --- | --- |
| `probe.ping` | reachability, touching no object |
| `probe.reset` | clears `sp/`, reports `remaining` (not `deleted` -- see below) |
| `probe.text` | put + get + `getText` round trip with a content type |
| `probe.binary` | the same for all 256 byte values, so a UTF-8 round trip somewhere in the stack corrupts the high half and the checksum says so |
| `probe.overwrite` | rewriting a key replaces it: new size (shorter), new content type, still one entry |
| `probe.deleteAbsent` | get-after-delete shape, and what a delete of an absent key reports |
| `probe.contentTypes` | four content types round-tripped, including the "not supplied" case |
| `probe.listPrefix` | prefix is a literal, not a glob; key order compared verbatim |
| `probe.listPaginate` | page boundaries at `limit: 2`, the truncation signal, and `listAll` agreeing with the manual loop |
| `probe.listOvershoot` | a limit larger than the key count returns one page and a null cursor |
| `probe.streamPut` | 1 MiB up in 16 chunks via `putStream`, checksummed as it is produced |
| `probe.streamGet` | the same object back via `getStream`, drained incrementally |
| `probe.streamGetAbsent` | a streaming read of a missing key reports absence, rather than throwing or handing back an empty stream |
| `probe.streamThenBuffered` | an object written by the streaming path read by the buffered path |

## Why the output is shaped the way it is

Every field has to be diffable between two backends, so nothing volatile
reaches the wire:

- **no timestamps** -- `modifiedAt` is dropped in the handler;
- **no cursors** -- documented opaque, and LocalFs and S3 mint different ones,
  so pagination is driven INSIDE the procedure and only the page boundaries and
  the truncation flags come back;
- **`reset` returns `remaining`, not `deleted`** -- how many objects it removed
  depends on what the previous run left behind, which would report a different
  STARTING STATE as a divergence.

The streaming payload is 1 MiB in 64 KiB chunks. Ten bytes would run the same
code with a single chunk and could not tell a chunked reader from a
whole-object one.

The checksum is position-weighted, so a dropped, duplicated, reordered or
short chunk changes it. Comparing lengths only would pass on a backend that
returned the right number of wrong bytes.

## Two things this app is shaped by

`src/index.ts` starts with `"use server";`. Without it the build succeeds,
prints `0 server functions`, and still emits a manifest declaring all fourteen
RPC resources -- so every procedure 404s at runtime with no build error (#167).
The harness greps the build log for `14 server functions` rather than trusting
the exit code.

`src/server/config.ts` opts every procedure into anonymous access. Without it
they resolve to `auth: "user"` and the gateway refuses them, which is green in
`pnpm dev` and 401 deployed -- the trap `examples/kv-dashboard` and
`examples/auth-uploads-kv` both shipped with (#163).
