"use server";

export interface UpstreamServiceErrorOptions {
  service: "control" | "sandbox";
  operation: string;
  status: number;
  body: string;
  publicMessage: string;
}

export class UpstreamServiceError extends Error {
  readonly service: string;
  readonly operation: string;
  readonly status: number;
  readonly request_id: string;
  readonly code = "UPSTREAM_REQUEST_FAILED";
  #upstreamBody: string;

  constructor(opts: UpstreamServiceErrorOptions) {
    const requestId = newRequestId();
    super(opts.publicMessage);
    this.name = "UpstreamServiceError";
    this.service = opts.service;
    this.operation = opts.operation;
    this.status = opts.status;
    this.request_id = requestId;
    this.#upstreamBody = opts.body;

    console.error("[zeroship-builder] upstream request failed", {
      request_id: requestId,
      service: opts.service,
      operation: opts.operation,
      status: opts.status,
      body: opts.body,
    });
  }
}

export function publicErrorWithRequestId(message: string, requestId: string): string {
  return `${message} (request_id=${requestId})`;
}

function newRequestId(): string {
  const cryptoLike = (globalThis as {
    crypto?: { randomUUID?: () => string };
  }).crypto;
  if (typeof cryptoLike?.randomUUID === "function") {
    return cryptoLike.randomUUID();
  }
  return `req_${Date.now().toString(36)}_${Math.random().toString(36).slice(2)}`;
}
