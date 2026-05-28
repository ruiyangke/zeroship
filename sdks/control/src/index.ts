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
  [key: string]: unknown;
}

export class ControlError extends Error {
  readonly status: number;
  readonly statusText: string;
  readonly body: unknown;
  readonly code?: string;
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

export interface AuthUser {
  id: string;
  email: string;
  name: string;
  avatar_url: string | null;
}

export interface RegisterInput {
  email: string;
  password: string;
  name: string;
}

export interface LoginInput {
  email: string;
  password: string;
  app_id?: string;
}

export interface AuthUserResult {
  user: AuthUser;
}

export interface UserInfo {
  user: AuthUser;
  app: string | null;
}

export interface ConsentInput {
  app_id: string;
}

export interface ConsentResult {
  granted: boolean;
  app_id: string;
}

export interface LogoutResult {
  logged_out: boolean;
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

  readonly auth = {
    register: (input: RegisterInput): Promise<AuthUserResult> =>
      this.request("/auth/register", { method: "POST", body: input }),
    login: (input: LoginInput): Promise<AuthUserResult> =>
      this.request("/auth/login", { method: "POST", body: input }),
    logout: (): Promise<LogoutResult> =>
      this.request("/auth/logout", { method: "POST" }),
    userinfo: (): Promise<UserInfo> => this.request("/auth/userinfo"),
    consent: (input: ConsentInput): Promise<ConsentResult> =>
      this.request("/auth/consent", { method: "POST", body: input }),
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
