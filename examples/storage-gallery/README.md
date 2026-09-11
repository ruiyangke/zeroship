# Storage gallery

This example exposes `@zeroship/storage` operations through the `gallery.*` RPC
procedures. Its public demo has no end-user accounts; the platform scopes its
objects to the app. The browser entry imports those procedures so Vite includes
them in the deploy artifact.

## Tests

```sh
pnpm test
pnpm typecheck
```

Vitest owns the acceptance suite, and Playwright checks browser loading and RPC
access. The fixture builds this example in a temporary directory, builds the
platform binaries, and starts Postgres, MinIO and a test issuer through
Testcontainers. It applies the platform migrations, creates an app, and deploys
through the CLI. Control, gateway and worker use S3 for deploy blobs; the worker
also uses S3 for app objects. A local Vite process exercises LocalFs.

The suite checks CRUD, binary content, metadata, multipart checksums and a
stream crossing the `u32` length boundary. The large-stream test checks the
physical S3 object's length and samples worker RSS across upload and download.
The RSS check requires Linux.
It sends authenticated gateway service assertions directly to the worker for
the long transfer; browser and ordinary multipart tests go through the gateway.
It is included in ordinary `pnpm test`; allow time for the full transfer.

Docker and the repository's built SDKs are required. Install Chromium with
`pnpm exec playwright install chromium`, or provide
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH`. On NixOS the tests discover Chromium on
`PATH`, so `nix shell nixpkgs#chromium --command pnpm test` also works.

Logs and failure screenshots live under `tests/.artifacts/`. Each run owns its
ports, containers, processes and temporary files. No shared database or
prebuilt example bundle is required.
