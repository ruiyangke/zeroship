# Builder deploy tool artifact path

## Status

Accepted, 2026-05-26.

## Context

The Builder agent edits a multi-file app inside a zeroship sandbox. The old
dashboard-side `deployApp` helper posted a JavaScript string to
`POST /api/apps/{id}/deploy`, but the current control-plane endpoint no longer
accepts raw JavaScript. `crates/control/src/api.rs` requires
`Content-Type: application/x-zship`, streams the body into a temporary `.zship`
file, and calls `zeroship_bundle::ingest`. `crates/bundle/src/unpack.rs`
expects a zstd-compressed tar archive with `manifest.json` first followed by
`blobs/<sha256>` entries, validates manifest version `1`, verifies every
referenced blob, computes the canonical `deploy_hash`, stores blobs and the
manifest, and lets the control plane atomically persist `deploy_hash` plus
`manifest_json` on the app row.

The canonical app build path is the `@zeroship/vite-plugin` production build.
During `vite build`, the plugin builds client assets, runs the SSR/server build
when a server entry exists, and emits `dist/app.zship` from the resulting
`dist/` tree.

## Decision

The Builder `deploy` tool deploys the sandbox project through the same artifact
path as the CLI:

1. Snapshot the sandbox source for review.
2. Invoke the Reviewer model inside the tool using `REVIEWER_PROMPT` and
   `reviewerResponseSchema`.
3. If `approved !== true`, return the blockers and do not build or upload.
4. If approved, run the project build inside the sandbox via
   `ZeroshipSandboxBackend.execute`.
5. Read `dist/app.zship` from the sandbox via the backend file download path.
6. POST those bytes to
   `POST /api/apps/{appId}/deploy` with `Content-Type: application/x-zship`
   and the configured control-plane bearer key.
7. Return the control-plane `deploy_hash` and the app URL.

The build command is package-manager aware but keeps the artifact contract
fixed: `pnpm build` when `pnpm-lock.yaml` and `pnpm` are present, `bun run
build` for `bun.lockb`, `yarn build` for `yarn.lock`, otherwise `npm run
build`. All branches must produce `dist/app.zship`.

## Rejected Options

- Raw JavaScript upload to `/api/apps/{id}/deploy`: rejected because the
  control-plane handler now returns `415` unless the content type is
  `application/x-zship`.
- Building a `.zship` inside the Builder server process: rejected because the
  source of truth is the sandbox filesystem. Building outside the sandbox risks
  deploying code different from what the agent wrote and tested.
- Letting the LLM call the Reviewer subagent before a separate deploy action:
  rejected because prompt-level sequencing is bypassable. The deploy tool owns
  the Reviewer call and checks `approved` before any upload.
- Uploading an existing `dist/app.zship` without running a build: rejected for
  normal deploys because it can ship a stale artifact. The tool always runs the
  sandbox build first and then reads the resulting archive.
