//
// RpcError — typed error thrown by every client call when the wire
// returns 4xx/5xx, the auth resolver fails, the network drops, or any
// underlying transport problem occurs.
//
// Wire envelope (`docs/proposals/rpc.md` §6, "Errors"):
//
//   { code, message, details?, retryable, trace_id? }
//
// `parseErrorResponse` walks the failure-shaped Response and returns an
// `RpcError`. When the body is structured (`application/zs-error+json`
// or plain JSON with the right keys), it lifts the envelope verbatim;
// otherwise it falls back to status-derived defaults so users still get
// a useful code instead of a bare HTTP number.

/**
 * Fixed gRPC-inspired enum. `docs/proposals/rpc.md` §6 defines the
 * wire strings; the client never invents codes.
 */
export type ErrorCode =
  | "UNAUTHENTICATED"
  | "PERMISSION_DENIED"
  | "NOT_FOUND"
  | "INVALID_ARGUMENT"
  | "FAILED_PRECONDITION"
  | "ALREADY_EXISTS"
  | "RESOURCE_EXHAUSTED"
  | "ABORTED"
  | "INTERNAL"
  | "UNAVAILABLE"
  | "TIMEOUT"
  | "CANCELLED"
  | "OUT_OF_RANGE"
  | "UNIMPLEMENTED";

/** Static enum-like alias users can import as `ErrorCode.NOT_FOUND`. */
export const ErrorCode: Readonly<Record<ErrorCode, ErrorCode>> = Object.freeze({
  UNAUTHENTICATED: "UNAUTHENTICATED",
  PERMISSION_DENIED: "PERMISSION_DENIED",
  NOT_FOUND: "NOT_FOUND",
  INVALID_ARGUMENT: "INVALID_ARGUMENT",
  FAILED_PRECONDITION: "FAILED_PRECONDITION",
  ALREADY_EXISTS: "ALREADY_EXISTS",
  RESOURCE_EXHAUSTED: "RESOURCE_EXHAUSTED",
  ABORTED: "ABORTED",
  INTERNAL: "INTERNAL",
  UNAVAILABLE: "UNAVAILABLE",
  TIMEOUT: "TIMEOUT",
  CANCELLED: "CANCELLED",
  OUT_OF_RANGE: "OUT_OF_RANGE",
  UNIMPLEMENTED: "UNIMPLEMENTED",
});

/** Default retryability from `docs/proposals/rpc.md` §6. */
const RETRYABLE_BY_CODE: Readonly<Record<ErrorCode, boolean>> = Object.freeze({
  UNAUTHENTICATED: false,
  PERMISSION_DENIED: false,
  NOT_FOUND: false,
  INVALID_ARGUMENT: false,
  FAILED_PRECONDITION: false,
  ALREADY_EXISTS: false,
  RESOURCE_EXHAUSTED: true,
  ABORTED: false,
  INTERNAL: false,
  UNAVAILABLE: true,
  TIMEOUT: true,
  CANCELLED: false,
  OUT_OF_RANGE: false,
  UNIMPLEMENTED: false,
});

/** Map HTTP status to the canonical RPC code. */
function codeFromStatus(status: number): ErrorCode {
  switch (status) {
    case 401:
      return "UNAUTHENTICATED";
    case 403:
      return "PERMISSION_DENIED";
    case 404:
      return "NOT_FOUND";
    case 409:
      return "ALREADY_EXISTS";
    case 413:
      return "INVALID_ARGUMENT";
    case 429:
      return "RESOURCE_EXHAUSTED";
    case 499:
      return "CANCELLED";
    case 503:
      return "UNAVAILABLE";
    case 504:
      return "TIMEOUT";
    case 501:
      return "UNIMPLEMENTED";
    default:
      if (status >= 400 && status < 500) return "INVALID_ARGUMENT";
      return "INTERNAL";
  }
}

/** Optional fields the wire envelope may carry. */
export interface RpcErrorInit {
  code: ErrorCode;
  message: string;
  details?: unknown;
  retryable?: boolean;
  /** HTTP status when the error originated on the wire. */
  status?: number;
  /** Snake_case to match the wire field exactly. */
  trace_id?: string;
  traceId?: string;
}

/**
 * Error thrown by the client for every failure path — wire 4xx/5xx,
 * auth resolver failures, transport errors, abort signals.
 *
 * Discriminated by `name === "RpcError"`. The `code` carries the
 * canonical fault category; `retryable` follows the wire envelope
 * when present and falls back to the §6 default per code.
 */
export class RpcError extends Error {
  readonly name = "RpcError";
  readonly code: ErrorCode;
  readonly details?: unknown;
  readonly retryable: boolean;
  readonly status?: number;
  readonly trace_id?: string;

  constructor(init: RpcErrorInit) {
    super(init.message);
    this.code = init.code;
    this.details = init.details;
    this.retryable =
      init.retryable !== undefined ? init.retryable : RETRYABLE_BY_CODE[init.code];
    this.status = init.status;
    // Accept either `trace_id` (wire) or `traceId` (camelCase ergonomics).
    this.trace_id = init.trace_id ?? init.traceId;
  }
}

/** Type guard for RpcError instances. */
export function isRpcError(err: unknown): err is RpcError {
  if (!err || typeof err !== "object") return false;
  return err instanceof RpcError || (err as { name?: string }).name === "RpcError";
}

/**
 * Decode a failure Response (4xx/5xx) into an `RpcError`. Tolerates
 * three shapes:
 *
 *   - `application/zs-error+json` with the full envelope.
 *   - `application/json` carrying the same fields (legacy / synthetic
 *     entry shape).
 *   - Anything else — fall back to status-derived code + the body as
 *     the message string.
 */
export async function parseErrorResponse(res: Response): Promise<RpcError> {
  const ct = res.headers.get("Content-Type") ?? "";
  const isStructured =
    ct.includes("application/zs-error+json") || ct.includes("application/json");

  let envelope: Partial<RpcErrorInit> & { message?: string } = {};
  if (isStructured) {
    const text = await res.text();
    try {
      const parsed = JSON.parse(text) as Record<string, unknown>;
      // Lift only the keys we know about — don't trust the server to
      // omit accidental properties.
      if (typeof parsed.code === "string") {
        envelope.code = parsed.code as ErrorCode;
      }
      if (typeof parsed.message === "string") envelope.message = parsed.message;
      if ("details" in parsed) envelope.details = parsed.details;
      if (typeof parsed.retryable === "boolean") envelope.retryable = parsed.retryable;
      if (typeof parsed.trace_id === "string") {
        envelope.trace_id = parsed.trace_id;
      } else if (typeof parsed.traceId === "string") {
        envelope.trace_id = parsed.traceId as string;
      }
    } catch {
      // Body wasn't JSON; fall through to status-derived defaults.
      envelope.message = text || res.statusText;
    }
  } else {
    envelope.message = (await res.text()) || res.statusText;
  }

  const code: ErrorCode = envelope.code ?? codeFromStatus(res.status);
  const message =
    envelope.message ?? `RPC error: ${res.status} ${res.statusText || code}`;

  return new RpcError({
    code,
    message,
    details: envelope.details,
    retryable: envelope.retryable,
    status: res.status,
    trace_id: envelope.trace_id,
  });
}
