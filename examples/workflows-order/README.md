# Workflow order example

A minimal durable workflow app that shows the raw creator contract:

- `OrderWorkflow` extends `Workflow<Params, Output>`.
- The handler starts runs with `env.workflows.OrderWorkflow.start(...)`.
- The workflow uses `step.run`, `step.sleep`, `step.waitForSignal`, a child
  workflow, and compensators.
- The default export exposes `workflows` plus a small fetch handler.

## Typecheck

```bash
pnpm --filter zeroship-workflows-order-example typecheck
```

## Build and run locally

```bash
pnpm --filter zeroship-workflows-order-example build
ZEROSHIP_CONTROL_URL=http://localhost:9090 \
ZEROSHIP_CONTROL_KEY=<control-key> \
zeroship serve examples/workflows-order/dist/index.js --port 3000
```

Then start and approve an order:

```bash
curl -X POST http://localhost:3000/orders \
  -H 'content-type: application/json' \
  -d '{"orderId":"ord_demo","sku":"sku_hat","quantity":2}'

curl -X POST http://localhost:3000/orders/<runId>/approve \
  -H 'content-type: application/json' \
  -d '{"approved":true,"approvalCode":"demo-ok"}'
```

Check status:

```bash
curl http://localhost:3000/orders/<runId>
```
