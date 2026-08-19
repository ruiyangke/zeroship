export type MaybePromise<T> = T | Promise<T>;
export type ValueProvider<T> = T | (() => MaybePromise<T>);

export interface ControlClientOptions {
  /** Control-plane origin, for example `http://localhost:9090`. */
  baseUrl: string | URL;
  /** Fetch implementation. Defaults to `globalThis.fetch`. */
  fetch?: typeof fetch;
  /** Master key / bearer token provider. Values without a scheme become `Bearer <value>`. */
  auth?: ValueProvider<string | null | undefined>;
  /** Cookie header provider for server-side session forwarding. */
  cookie?: ValueProvider<string | null | undefined>;
  /** Global headers applied to every request. */
  headers?: ValueProvider<HeadersInit | null | undefined>;
  /** Called for every upstream `Set-Cookie` header. Useful for SSR/RPC proxies. */
  onSetCookie?: (cookie: string, response: Response) => MaybePromise<void>;
}

export interface ControlRequestOptions {
  method?: string;
  query?: Record<string, string | number | boolean | null | undefined>;
  headers?: HeadersInit;
  body?: unknown;
  /** `null` means do not set a content-type. Objects default to JSON. */
  contentType?: string | null;
  parseAs?: "json" | "text" | "void";
}

export interface ControlErrorBody {
  error?: unknown;
  message?: unknown;
  code?: unknown;
  /**
   * Server-minted correlation id. Only responses produced by
   * `infrastructure_error_response` carry this id. Failures such as
   * `control.env.listVars` can remain id-less. The helper's body is generic
   * by design (`{"error":"internal error"}`), and it logs the real cause
   * under the same key and the same value.
   */
  trace_id?: unknown;
  [key: string]: unknown;
}

export class ControlError extends Error {
  readonly status: number;
  readonly statusText: string;
  readonly body: unknown;
  readonly code?: string;
  /**
   * Correlation id lifted off the body. When present, quote it to an
   * operator: the producing helper logged the real cause under the same
   * key. `undefined` when the server did not send one -- never invented
   * here.
   *
   * Named `trace_id` to match the spelling `@zeroship/rpc` already lifts
   * (`sdks/rpc/src/error.ts`) rather than adding another id concept to a
   * system that has several that do not join.
   */
  readonly trace_id?: string;
  readonly response: Response;

  constructor(response: Response, body: unknown) {
    super(errorMessage(response, body));
    this.name = "ControlError";
    this.status = response.status;
    this.statusText = response.statusText;
    this.body = body;
    this.response = response;
    const code = isRecord(body) ? body.code : undefined;
    this.code = typeof code === "string" ? code : undefined;
    const traceId = isRecord(body) ? body.trace_id : undefined;
    this.trace_id = typeof traceId === "string" ? traceId : undefined;
  }
}

export interface AppRecord {
  id: string;
  name: string;
  plan_id: string;
  deploy_hash: string | null;
  /** Present on create/admin get responses; intentionally omitted from public list responses. */
  api_key?: string;
  created_at: string;
  updated_at: string;
}

export interface CreateAppInput {
  name: string;
  plan_id?: string;
}

export interface DeleteAppResult {
  deleted: boolean;
}

export interface DeployAppResult {
  deploy_hash: string;
  blobs_uploaded?: number;
  blobs_deduped?: number;
}

export interface SetPlanInput {
  plan_id: string;
}

export interface SetPlanResult {
  updated: boolean;
}

export type UsageCounters = Record<string, number>;

export interface EnvVar {
  key: string;
  value: string;
}

export interface ListVarsResult {
  vars: EnvVar[];
}

export interface ListSecretsResult {
  secrets: string[];
}

export interface ListExposeResult {
  expose: string[];
}

export interface SetExposeInput {
  keys: string[];
}

/**
 * What a rule does when it matches. There is no default: a rule that does not
 * say which one it is, is refused.
 */
export type EgressVerdict = "accept" | "reject";

/**
 * Which of the two things a destination is. The server infers it from the
 * destination's own grammar rather than taking it from you: a value that parses
 * as a CIDR range is `"cidr"`, anything else must parse as an exact DNS name
 * and is `"name"`.
 *
 * The distinction is not cosmetic. A name is decided before the app resolves
 * anything; a range can only be decided against a resolved address.
 */
export type EgressDestinationKind = "name" | "cidr";

/** One egress rule for an app. */
export interface EgressRule {
  app_id: string;
  verdict: EgressVerdict;
  kind: EgressDestinationKind;
  /** The exact DNS name, or the range in canonical CIDR form. */
  destination: string;
  port: number;
  created_by: string;
  created_at: string;
  note: string | null;
  /**
   * The verdict that actually applies once the whole rule set is read.
   * Rules are an unordered set and any matching reject wins, so an accept
   * range wholly inside a reject range at the same port reports
   * `"reject"` here while `verdict` still reads `"accept"`.
   *
   * Only statically decidable overlaps are reflected. Whether a name lands
   * inside a rejected range depends on what that name resolves to at connect
   * time, which this endpoint does not look up, so a name rule always reports
   * its own verdict.
   */
  effective_verdict: EgressVerdict;
}

/** A `net.requests` hint from the app's manifest. Inert until a rule exists. */
export interface EgressRequest {
  host: string;
  port: number;
  reason: string;
}

/** The plan ceiling on egress. A creator cannot raise any of these. */
export interface EgressRuleLimits {
  /** Bounds accept rules only. A reject rule can only narrow, so it is never charged here. */
  max_accept_rules: number;
  used_accept_rules: number;
  /**
   * Reject rules are capped separately, and for an unrelated reason: every
   * rule rides the projection the runtime polls, so an unbounded reject list
   * is a load problem. It is never a safety bound - a reject can only narrow.
   *
   * Declared because the server has always sent it. Omitting it here meant a
   * creator's rule set could 409 against a ceiling that appeared in no client
   * type and no doc.
   */
  max_reject_rules: number;
  used_reject_rules: number;
  max_sockets: number;
  egress_ceiling_bytes: number;
}

export interface ListEgressRulesResult {
  app_id: string;
  rules: EgressRule[];
  requests: EgressRequest[];
  /** Manifest hints with no matching accept rule: what is still refused. */
  pending_requests: EgressRequest[];
  limits: EgressRuleLimits;
}

export interface EgressRuleInput {
  /** Required. Omitting it is a 400 rather than an implied `"accept"`. */
  verdict: EgressVerdict;
  /** An exact DNS name, or an address range in CIDR form. No wildcards. */
  destination: string;
  port: number;
  note?: string;
}

export interface DeleteEgressRuleInput {
  destination: string;
  port: number;
}

export interface SetEgressRuleResult {
  rule: EgressRule;
  /**
   * A one-time explanation, non-null only on an app's FIRST accept rule for
   * an address range. That rule changes how the app refuses destinations it
   * does not allow, and this is the only place that is said. Show it.
   */
  notice: string | null;
}

export interface AuditEntry {
  id: string;
  actor: string;
  action: string;
  resource: string | null;
  source_ip: string | null;
  at: string;
}

export interface ListAuditOptions {
  limit?: number;
}

export interface ListAuditResult {
  audit: AuditEntry[];
  count: number;
  limit: number;
}

export interface SetKeyValueInput {
  key: string;
  value: string;
}

export type DeployBody =
  | string
  | ArrayBuffer
  | ArrayBufferView
  | Blob
  | FormData
  | URLSearchParams
  | ReadableStream<Uint8Array>;

export interface DeployOptions {
  contentType?: string;
}

export interface WorkflowSignalTokenInput {
  appId: string;
  types: string[];
  ttl: string;
}

export interface WorkflowSignalTokenResult {
  token: string;
  expiresAt: string;
}

export interface WorkflowTopicBroadcastInput {
  appId: string;
  type: string;
  payload?: unknown;
  idempotencyKey?: string;
}

export interface WorkflowTopicBroadcastResult {
  id: string;
  topic: string;
}

export class ControlClient {
  readonly #options: ControlClientOptions;
  readonly #fetch: typeof fetch;

  readonly apps = {
    list: (): Promise<AppRecord[]> => this.request("/api/apps"),
    get: (id: string): Promise<AppRecord> =>
      this.request(`/api/apps/${pathPart(id)}`),
    create: (input: CreateAppInput): Promise<AppRecord> =>
      this.request("/api/apps", {
        method: "POST",
        body: {
          name: input.name,
          plan_id: input.plan_id ?? "free",
        },
      }),
    delete: (id: string): Promise<DeleteAppResult> =>
      this.request(`/api/apps/${pathPart(id)}`, { method: "DELETE" }),
    deploy: (
      id: string,
      artifact: DeployBody,
      options: DeployOptions = {},
    ): Promise<DeployAppResult> =>
      this.request(`/api/apps/${pathPart(id)}/deploy`, {
        method: "POST",
        body: artifact,
        contentType: options.contentType ?? "application/x-zship",
      }),
    setPlan: (id: string, input: SetPlanInput): Promise<SetPlanResult> =>
      this.request(`/api/apps/${pathPart(id)}/plan`, {
        method: "PUT",
        body: input,
      }),
    usage: (id: string): Promise<UsageCounters> =>
      this.request(`/api/apps/${pathPart(id)}/usage`),
    logs: (id: string): Promise<string[]> =>
      this.request(`/api/apps/${pathPart(id)}/logs`),
  };

  readonly env = {
    listVars: (appId: string): Promise<ListVarsResult> =>
      this.request(`/api/apps/${pathPart(appId)}/vars`),
    setVar: (appId: string, input: SetKeyValueInput): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/vars`, {
        method: "POST",
        body: input,
        parseAs: "void",
      }),
    deleteVar: (appId: string, key: string): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/vars/${pathPart(key)}`, {
        method: "DELETE",
        parseAs: "void",
      }),
    listSecrets: (appId: string): Promise<ListSecretsResult> =>
      this.request(`/api/apps/${pathPart(appId)}/secrets`),
    setSecret: (appId: string, input: SetKeyValueInput): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/secrets`, {
        method: "POST",
        body: input,
        parseAs: "void",
      }),
    deleteSecret: (appId: string, key: string): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/secrets/${pathPart(key)}`, {
        method: "DELETE",
        parseAs: "void",
      }),
    listExpose: (appId: string): Promise<ListExposeResult> =>
      this.request(`/api/apps/${pathPart(appId)}/env/expose`),
    setExpose: (appId: string, input: SetExposeInput): Promise<ListExposeResult> =>
      this.request(`/api/apps/${pathPart(appId)}/env/expose`, {
        method: "PUT",
        body: input,
      }),
    listAudit: (
      appId: string,
      options: ListAuditOptions = {},
    ): Promise<ListAuditResult> =>
      this.request(`/api/apps/${pathPart(appId)}/audit`, {
        query: { limit: options.limit },
      }),
  };

  /**
   * The app's egress rules. They cover every raw byte stream the app can open
   * - `node:net`, `node:tls` and outbound `WebSocket` - through one rule set.
   * An app with no accept rule opens none of them. `fetch` is the exception
   * and reaches any public host with no rule at all.
   *
   * A rule is a verdict, a destination and a port. The destination is either an
   * exact DNS name (`api.example.com`) or an address range in CIDR form
   * (`93.184.216.0/24`); wildcards such as `*.example.com` are not a grammar
   * this API accepts. Rules are an unordered set, not an ordered list: any
   * matching reject wins, then any matching accept admits, and anything else is
   * refused.
   *
   * Use real public space in a range, not the documentation ranges
   * (`198.51.100.0/24`, `2001:db8::/32`): those are refused by the platform
   * SSRF floor, so such a rule is accepted here and can never admit a connect.
   *
   * Same authority as `env`: `env:read` to list, `env:write` to change.
   */
  readonly egressRules = {
    list: (appId: string): Promise<ListEgressRulesResult> =>
      this.request(`/api/apps/${pathPart(appId)}/egress-rules`),
    set: (appId: string, input: EgressRuleInput): Promise<SetEgressRuleResult> =>
      this.request(`/api/apps/${pathPart(appId)}/egress-rules`, {
        method: "POST",
        body: input,
      }),
    remove: (appId: string, input: DeleteEgressRuleInput): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/egress-rules`, {
        method: "DELETE",
        body: input,
        parseAs: "void",
      }),
  };

  readonly workflows = {
    createSignalToken: (
      runId: string,
      input: WorkflowSignalTokenInput,
    ): Promise<WorkflowSignalTokenResult> =>
      this.request(`/internal/workflows/runs/${pathPart(runId)}/signal-token`, {
        method: "POST",
        headers: { "x-zeroship-app-id": input.appId },
        body: {
          types: input.types,
          ttl: input.ttl,
        },
      }),
    createTopicSignalToken: (
      topic: string,
      input: WorkflowSignalTokenInput,
    ): Promise<WorkflowSignalTokenResult> =>
      this.request(`/internal/workflows/topics/${pathPart(topic)}/signal-token`, {
        method: "POST",
        headers: { "x-zeroship-app-id": input.appId },
        body: {
          types: input.types,
          ttl: input.ttl,
        },
      }),
    publishTopic: (
      topic: string,
      input: WorkflowTopicBroadcastInput,
    ): Promise<WorkflowTopicBroadcastResult> =>
      this.request(`/internal/workflows/topics/${pathPart(topic)}/broadcast`, {
        method: "POST",
        headers: { "x-zeroship-app-id": input.appId },
        body: {
          type: input.type,
          payload: input.payload,
          idempotencyKey: input.idempotencyKey,
        },
      }),
  };

  constructor(options: ControlClientOptions) {
    if (!options.baseUrl) {
      throw new Error("createControlClient requires a baseUrl");
    }
    const fetchImpl = options.fetch ?? globalThis.fetch;
    if (!fetchImpl) {
      throw new Error("createControlClient requires a fetch implementation");
    }
    this.#options = options;
    this.#fetch = fetchImpl;
  }

  async request<T = unknown>(
    path: string,
    options: ControlRequestOptions = {},
  ): Promise<T> {
    const headers = await this.#headers(options);
    const body = encodeBody(options.body, headers, options.contentType);

    const response = await this.#fetch(this.#url(path, options.query), {
      method: options.method ?? "GET",
      headers,
      body,
    });

    await this.#forwardSetCookie(response);

    const parsed = await parseResponseBody(response, options.parseAs);
    if (!response.ok) {
      throw new ControlError(response, parsed);
    }
    return parsed as T;
  }

  async #headers(options: ControlRequestOptions): Promise<Headers> {
    const globalHeaders = await resolveProvider(this.#options.headers);
    const headers = new Headers(globalHeaders ?? undefined);
    const auth = await resolveProvider(this.#options.auth);
    const cookie = await resolveProvider(this.#options.cookie);

    if (auth) {
      headers.set("authorization", authHeader(auth));
    }
    if (cookie) {
      headers.set("cookie", cookie);
    }
    mergeHeaders(headers, options.headers);
    return headers;
  }

  #url(path: string, query?: ControlRequestOptions["query"]): string {
    const base = this.#options.baseUrl.toString();
    const url = new URL(path, base.endsWith("/") ? base : `${base}/`);
    if (query) {
      for (const [key, value] of Object.entries(query)) {
        if (value !== undefined && value !== null) {
          url.searchParams.set(key, String(value));
        }
      }
    }
    return url.toString();
  }

  async #forwardSetCookie(response: Response): Promise<void> {
    if (!this.#options.onSetCookie) return;
    for (const cookie of readSetCookies(response.headers)) {
      await this.#options.onSetCookie(cookie, response);
    }
  }
}

export function createControlClient(options: ControlClientOptions): ControlClient {
  return new ControlClient(options);
}

async function resolveProvider<T>(
  provider: ValueProvider<T> | null | undefined,
): Promise<T | undefined> {
  if (provider === undefined || provider === null) return undefined;
  if (typeof provider === "function") {
    return (provider as () => MaybePromise<T>)();
  }
  return provider;
}

function mergeHeaders(headers: Headers, input: HeadersInit | undefined): void {
  if (!input) return;
  new Headers(input).forEach((value, key) => headers.set(key, value));
}

function authHeader(value: string): string {
  return /^[a-z]+ /i.test(value) ? value : `Bearer ${value}`;
}

function encodeBody(
  body: unknown,
  headers: Headers,
  contentType: string | null | undefined,
): BodyInit | undefined {
  if (body === undefined) return undefined;
  if (shouldJsonEncode(body)) {
    setContentType(headers, contentType ?? "application/json");
    return JSON.stringify(body);
  }
  setContentType(headers, contentType);
  return body as BodyInit;
}

function shouldJsonEncode(body: unknown): body is Record<string, unknown> {
  if (body === null || typeof body !== "object") return false;
  if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) return false;
  if (typeof Blob !== "undefined" && body instanceof Blob) return false;
  if (typeof FormData !== "undefined" && body instanceof FormData) return false;
  if (typeof URLSearchParams !== "undefined" && body instanceof URLSearchParams) {
    return false;
  }
  if (typeof ReadableStream !== "undefined" && body instanceof ReadableStream) {
    return false;
  }
  return true;
}

function setContentType(
  headers: Headers,
  contentType: string | null | undefined,
): void {
  if (contentType === undefined || contentType === null) return;
  if (!headers.has("content-type")) {
    headers.set("content-type", contentType);
  }
}

async function parseResponseBody(
  response: Response,
  parseAs: ControlRequestOptions["parseAs"],
): Promise<unknown> {
  if (parseAs === "void" || response.status === 204) return undefined;
  if (parseAs === "text") return response.text();
  const contentType = response.headers.get("content-type") ?? "";
  if (parseAs === "json" || contentType.includes("json")) {
    return response.json().catch(() => undefined);
  }
  return response.text();
}

function readSetCookies(headers: Headers): string[] {
  const withGetSetCookie = headers as Headers & {
    getSetCookie?: () => string[];
  };
  const values = withGetSetCookie.getSetCookie?.();
  if (values && values.length > 0) return values;

  const single = headers.get("set-cookie");
  return single ? [single] : [];
}

function errorMessage(response: Response, body: unknown): string {
  if (isRecord(body)) {
    if (typeof body.error === "string") return body.error;
    if (typeof body.message === "string") return body.message;
  }
  if (typeof body === "string" && body.length > 0) return body;
  return `HTTP ${response.status}`;
}

function isRecord(value: unknown): value is ControlErrorBody {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function pathPart(value: string): string {
  return encodeURIComponent(value);
}
