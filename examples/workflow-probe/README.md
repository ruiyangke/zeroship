# workflow-probe

A runnable app for journaled steps, sleeps, signals, child workflows, and
compensation. Each RPC handler performs a short operation; the caller waits
for durable progress by polling status.

```sh
pnpm dev
pnpm build
pnpm test
```

The example owns its Vitest tests, Playwright browser checks, and platform
fixtures under `tests/`. The fixture builds this app and the real platform
binaries, starts PostgreSQL and Redis through Testcontainers, applies platform
migrations, provisions the journal through the migration service, deploys the
app, and runs the same cases locally and through the
gateway. Docker and the built workspace SDKs are required. Failure logs and
browser screenshots remain under `tests/.artifacts/`.

The tests assert results independently on each tier, observe sleeps and signal
waits before resuming them, and check compensation through its KV trail. The
browser test loads the built assets and calls the app over its own origin.

Local development currently has explicit limitations:

- Child workflows fail with `WorkflowUnsupportedError`.
- Compensators do not run. The error preserves the creator's failure and reports
  `compensation.supported: false` with `outcome: "not-attempted"`.

The tests check those limits and deployed child/compensation behavior. Changes
to local support require updating the assertions along with the implementation.

The RPC procedures are `wf.ping`, `wf.start`, `wf.status`, `wf.signal`,
`wf.trail`, and `wf.resetTrail`. The supported cases are `basic`, `sleep`,
`signal`, `child`, and `compensate`.
