import { Workflow, PermanentError, type Step, type WorkflowRun, type WorkflowTrigger } from "@zeroship/workflows";

type OrderInput = {
  orderId: string;
  sku: string;
  quantity: number;
};

type LoadedOrder = OrderInput & {
  correlationId: string;
  totalCents: number;
};

type RiskInput = {
  orderId: string;
  totalCents: number;
};

type RiskOutput = {
  approved: boolean;
  score: number;
  reason?: string;
};

type PaymentSignal = {
  approved: boolean;
  approvalCode?: string;
};

type Reservation = {
  reservationId: string;
  sku: string;
  quantity: number;
};

type Shipment = {
  shipmentId: string;
  carrierRef: string;
};

type OrderOutput = {
  orderId: string;
  status: "ready_to_ship";
  shipmentId: string;
  riskScore: number;
};

type WorkflowStartOptions<Input> = {
  input: Input;
  key?: string;
  onConflict?: "join" | "reject" | "replace" | { policy: "join" | "reject" | "replace" };
};

type WorkflowHandle<Input, Output> = {
  start(opts: WorkflowStartOptions<Input>): Promise<WorkflowRun<Output>>;
  get(runId: string): WorkflowRun<Output>;
};

type WorkflowEnv = {
  workflows: {
    OrderWorkflow: WorkflowHandle<OrderInput, OrderOutput>;
  };
};

export class RiskReviewWorkflow extends Workflow<RiskInput, RiskOutput> {
  async run(trigger: WorkflowTrigger<RiskInput>, step: Step): Promise<RiskOutput> {
    const score = await step.sideEffect("score", () => Math.floor(Math.random() * 100));
    await step.sleep("review-window", "2s");

    if (trigger.input.totalCents < 50_000 || score < 80) {
      return { approved: true, score };
    }

    return {
      approved: false,
      score,
      reason: `risk score ${score} requires manual handling`,
    };
  }
}

export class OrderWorkflow extends Workflow<OrderInput, OrderOutput> {
  async run(trigger: WorkflowTrigger<OrderInput>, step: Step): Promise<OrderOutput> {
    const correlationId = await step.sideEffect("correlation-id", () => crypto.randomUUID());

    const order = await step.run("load-order", () =>
      loadOrder(trigger.input, correlationId)
    );

    const risk = await step.call(
      RiskReviewWorkflow,
      { orderId: order.orderId, totalCents: order.totalCents },
      { cascade: true, timeout: "5m" },
    );
    if (!risk.approved) {
      throw new PermanentError(risk.reason ?? "order did not pass review");
    }

    const reservation = await step.run(
      "reserve-inventory",
      {
        timeout: "10s",
        compensate: (output: Reservation, ctx) =>
          releaseInventory(output, ctx.idempotencyKey),
      },
      () => reserveInventory(order),
    );

    await step.sleep("customer-change-window", "10s");

    const payment = await step.waitForSignal<PaymentSignal>("payment-approved", {
      type: "payment.approved",
      timeout: "30m",
      maxSignalAge: "10m",
    });
    if (!payment?.payload.approved) {
      throw new PermanentError("payment approval window elapsed");
    }

    const shipment = await step.run(
      "create-shipment",
      {
        timeout: "15s",
        compensate: (output: Shipment, ctx) =>
          cancelShipment(output, ctx.idempotencyKey),
      },
      () => createShipment(order, reservation, payment.payload),
    );

    return {
      orderId: order.orderId,
      status: "ready_to_ship",
      shipmentId: shipment.shipmentId,
      riskScore: risk.score,
    };
  }
}

async function fetch(request: Request, rawEnv: unknown): Promise<Response> {
  const url = new URL(request.url);
  const env = rawEnv as WorkflowEnv;

  if (request.method === "GET" && url.pathname === "/") {
    return json({
      routes: [
        "POST /orders",
        "GET /orders/{runId}",
        "POST /orders/{runId}/approve",
      ],
    });
  }

  if (request.method === "POST" && url.pathname === "/orders") {
    const input = await readJson<OrderInput>(request);
    const run = await env.workflows.OrderWorkflow.start({
      input,
      key: `order:${input.orderId}`,
      onConflict: "join",
    });
    return json({ runId: run.id }, 202);
  }

  const statusMatch = url.pathname.match(/^\/orders\/([^/]+)$/);
  if (request.method === "GET" && statusMatch) {
    const runId = decodeURIComponent(statusMatch[1]!);
    const status = await env.workflows.OrderWorkflow.get(runId).status();
    return json({ runId, ...status });
  }

  const approveMatch = url.pathname.match(/^\/orders\/([^/]+)\/approve$/);
  if (request.method === "POST" && approveMatch) {
    const runId = decodeURIComponent(approveMatch[1]!);
    const payload = await readJson<PaymentSignal>(request);
    const run = env.workflows.OrderWorkflow.get(runId);
    await run.signal({
      type: "payment.approved",
      payload: {
        approved: payload.approved === true,
        approvalCode: payload.approvalCode,
      },
      idempotencyKey: payload.approvalCode,
    });
    const status = await run.status();
    return json({ runId, ...status });
  }

  return json({ error: "not found" }, 404);
}

async function loadOrder(input: OrderInput, correlationId: string): Promise<LoadedOrder> {
  return {
    ...input,
    correlationId,
    totalCents: input.quantity * 2_500,
  };
}

async function reserveInventory(order: LoadedOrder): Promise<Reservation> {
  return {
    reservationId: `res_${order.correlationId}`,
    sku: order.sku,
    quantity: order.quantity,
  };
}

async function releaseInventory(
  reservation: Reservation,
  idempotencyKey: string,
): Promise<void> {
  console.log("release inventory", {
    idempotencyKey,
    reservationId: reservation.reservationId,
  });
}

async function createShipment(
  order: LoadedOrder,
  reservation: Reservation,
  payment: PaymentSignal,
): Promise<Shipment> {
  return {
    shipmentId: `shp_${order.orderId}`,
    carrierRef: `${reservation.reservationId}:${payment.approvalCode ?? "approved"}`,
  };
}

async function cancelShipment(shipment: Shipment, idempotencyKey: string): Promise<void> {
  console.log("cancel shipment", {
    idempotencyKey,
    shipmentId: shipment.shipmentId,
  });
}

async function readJson<T>(request: Request): Promise<T> {
  return request.json() as Promise<T>;
}

function json(body: unknown, status = 200): Response {
  return Response.json(body, { status });
}

export default {
  workflows: {
    OrderWorkflow,
    RiskReviewWorkflow,
  },
  fetch,
};
