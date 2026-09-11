# Storage probe

A public demonstration of `@zeroship/storage` with no end-user accounts.
Objects remain scoped to the app, and the probe uses the `sp/` key prefix.

| Procedures | Behavior |
| --- | --- |
| `probe.ping`, `probe.reset` | Reachability and clearing the probe's objects |
| `probe.text`, `probe.binary` | Text and arbitrary-byte round trips |
| `probe.overwrite`, `probe.deleteAbsent` | Replacement, deletion and missing objects |
| `probe.contentTypes` | Stored content types |
| `probe.listPrefix`, `probe.listPaginate`, `probe.listOvershoot` | Literal prefixes, ordering, page boundaries and cursors |
| `probe.streamPut`, `probe.streamGet` | Chunked transfers and matching checksums |
| `probe.streamGetAbsent`, `probe.streamThenBuffered` | Missing streams and interoperability with buffered reads |

## Tests

```sh
pnpm test
pnpm typecheck
```

Vitest asserts the operation contract independently for local development and
S3 deployment, then compares their answers. Playwright loads the browser entry
and reaches storage through RPC. Each run builds this example in a temporary
directory and builds the platform binaries. Its Testcontainers fixture owns
Postgres, MinIO and a test issuer; it applies migrations and deploys through
the CLI. Deploy artifacts also live in S3.

Docker and the repository's built SDKs are required. Install Chromium with
`pnpm exec playwright install chromium`, or provide
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH`. On NixOS the tests discover Chromium on
`PATH`: `nix shell nixpkgs#chromium --command pnpm test`.

Logs and failure screenshots live under `tests/.artifacts/`. The example owns
its test fixtures and configuration, with no shared database or prebuilt
example bundle required.
