# Workflow order example

A raw JavaScript deploy contract for an order workflow. `OrderWorkflow` loads
an order, calls `RiskReviewWorkflow`, reserves inventory, waits for payment
approval, and creates a shipment. Reservations and shipments have compensators.
The default export exposes the workflow classes and an HTTP fetch handler.

```sh
pnpm build
zeroship serve dist/index.js --port 3000
pnpm test
```

Vite bundles the SDK into `dist/index.js`. Local development uses the SQLite
workflow engine. Its child-workflow limitation means an order currently ends
with `WorkflowUnsupportedError` locally; the deployed engine runs the full
order flow.

The example owns its Vitest and Playwright tests under `tests/`. Its fixture
builds a raw `.zship`, starts PostgreSQL through Testcontainers, applies platform
migrations, and deploys to real control, gateway, and worker processes. Docker
and the built workspace SDKs are required. Tests cover duplicate starts,
approval signals, shipment results, and browser requests. Logs and screenshots
remain under `tests/.artifacts/`.

HTTP routes:

- `POST /orders`: start or join an order using `orderId`, `sku`, and `quantity`.
- `GET /orders/{runId}`: read durable state and output.
- `POST /orders/{runId}/approve`: send `approved` and an optional `approvalCode`.
