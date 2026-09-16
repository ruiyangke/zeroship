# Workflow order example

A raw JavaScript deploy contract for an order workflow. `OrderWorkflow` loads
an order, calls `RiskReviewWorkflow`, reserves inventory, waits for payment
approval, and creates a shipment. Reservations and shipments have compensators.
The default export exposes the workflow classes and an HTTP fetch handler.

```sh
pnpm build
zeroship serve dist/app.zship --port 3000
pnpm test
```

The zeroship Vite plugin builds the ordinary app bundle at `dist/app.zship`,
including its workflow definitions. Local development uses the SQLite workflow
engine and executes the same child, approval, and shipment flow as deployment.

The example owns its Vitest and Playwright tests under `tests/`. Its fixture
builds the app bundle, starts PostgreSQL through Testcontainers, applies platform
migrations, and deploys to real control, gateway, and worker processes. Docker
and the built workspace SDKs are required. Tests cover duplicate starts,
approval signals, shipment results, and browser requests. Logs and screenshots
remain under `tests/.artifacts/`.

HTTP routes:

- `POST /orders`: start or join an order using `orderId`, `sku`, and `quantity`.
- `GET /orders/{runId}`: read durable state and output.
- `POST /orders/{runId}/approve`: send `approved` and an optional `approvalCode`.
