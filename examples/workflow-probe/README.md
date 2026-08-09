# workflow-probe

A runnable app that exercises the durable-workflow primitives which actually
distinguish workflows from a background job: **start**, **step.sleep**,
**step.waitForSignal**, **step.call** (child workflows), and **compensation**.

It exists to be driven on both sides of the seam by
`tests/e2e_dev_vs_deployed_workflows.sh`, which runs the same five cases under
`pnpm dev` and against the same app deployed behind the gateway and diffs the
results.

## Why it was written

`tests/e2e_durable_workflows.sh` already drives the deployed workflow engine
hard, but it builds its app from an inline heredoc and hand-packs the `.zship`,
including a `"workflows":[...]` array it writes itself. That proves the engine
runs workflows; it cannot see whether a workflow authored in a normal vite app
ever reaches the engine. `examples/workflows-order` is the shipped sample and is
typecheck-only (#166) - no vite config, no dev script, no build, in no test.

Walking the gap between them turned up three defects that were invisible to
both instruments, all now fixed:

- the vite build never emitted `manifest.workflows`, so the control plane
  refused every start with *"workflow 'X' is not declared by the active
  deploy"*;
- the synthetic server entry and the dev bootstrap both exposed only
  `default = { fetch, rpc }`, so workflow classes were unreachable from the
  runtime's dispatch and every run failed with *"Workflow not found"*;
- production minification renames `class DoubleChild` to `var Pi = class ...`,
  and `step.call` addresses a child by `Class.name`, so child workflows
  addressed a minified identifier in a `.zship` and the source name in dev.

## Running

```bash
pnpm install
pnpm dev      # http://localhost:3051  (explicit port: the 3001 default collides, #173)
pnpm build    # dist/app.zship
```

There is no UI. Drive the RPC endpoints:

```bash
curl -sX POST localhost:3051/__zeroship/v1/wf.ping        -d '{"json":{}}'
curl -sX POST localhost:3051/__zeroship/v1/wf.start       -d '{"json":{"case":"basic"}}'
curl -sX POST localhost:3051/__zeroship/v1/wf.status      -d '{"json":{"workflow":"BasicCase","runId":"run_..."}}'
```

Cases: `basic`, `sleep`, `signal`, `child`, `compensate`.

## Two things this app is deliberately shaped around

**Every procedure is one short operation.** An earlier version polled a run to
completion inside a single mutation. That works in dev and returns
`{"message":"request timed out"}` for every case deployed: `zeroship serve`
leaves the per-request wall clock unbounded, while a deployed app inherits the
free-tier limits of 5s wall and 50ms CPU. Anything that waits belongs in the
caller, not in a handler.

**Compensation is observed through `env.kv`.** `run.status()` returns state,
output and error, and says nothing about whether a compensator fired, so the
compensable step and its compensator each append a marker to a kv key that
`wf.trail` reads back. `do:reserve` alone means the rollback never happened.

## Known dev-tier gaps this app makes visible

Both are dev-engine limitations, not bugs in the app, and both are asserted
explicitly by the comparison script rather than papered over:

- **`step.call` does not work under `pnpm dev`.** The local mini-engine rejects
  every child checkpoint with `WorkflowUnsupportedError`. Deployed it works.
- **Compensators never run under `pnpm dev`.** The local engine has no
  compensating phase, so a compensable step that fails is left un-rolled-back
  with no error and no warning. Deployed the compensator runs.
