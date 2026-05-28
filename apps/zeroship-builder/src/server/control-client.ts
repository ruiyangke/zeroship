"use server";

import { refreshAccessToken } from "./oauth.js";
import { loadTokens, saveTokens } from "./oauth-store.js";
import { getRequest } from "./internal/request-context.js";
import { userIdFromRequest } from "./session.js";

export interface ControlClientOptions {
  userId: string;
  baseUrl?: string;
}

export interface App {
  id: string;
  name: string;
  plan_id: string;
  deploy_hash: string | null;
  api_key: string;
  created_at: string;
  updated_at: string;
  server_js?: string;
}

export interface DeployResult {
  deploy_hash: string;
  blobs_uploaded?: number;
  blobs_deduped?: number;
}

export interface EnvVar {
  key: string;
  value: string;
}

export class OauthExpiredError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "OauthExpiredError";
  }
}

export class ControlApiError extends Error {
  readonly status: number;
  readonly body: string;

  constructor(status: number, body: string) {
    super(body || `control API failed (${status})`);
    this.name = "ControlApiError";
    this.status = status;
    this.body = body;
  }
}

export class ControlClient {
  private readonly userId: string;
  private readonly baseUrl: string;

  constructor(opts: ControlClientOptions) {
    if (!opts.userId) throw new Error("userId is required");
    this.userId = opts.userId;
    this.baseUrl = stripTrailingSlash(opts.baseUrl ?? controlBaseUrl());
  }

  async listApps(): Promise<App[]> {
    return this.fetchJson<App[]>("GET", "/api/apps");
  }

  async getApp(id: string): Promise<App> {
    return this.fetchJson<App>("GET", `/api/apps/${encodeURIComponent(id)}`);
  }

  async createApp(name: string, planId: string): Promise<App> {
    return this.fetchJson<App>("POST", "/api/apps", { name, plan_id: planId });
  }

  async deleteApp(id: string): Promise<{ deleted: boolean }> {
    return this.fetchJson<{ deleted: boolean }>("DELETE", `/api/apps/${encodeURIComponent(id)}`);
  }

  async deploy(appId: string, zshipBytes: Uint8Array): Promise<DeployResult> {
    return this.fetchRaw<DeployResult>(
      "POST",
      `/api/apps/${encodeURIComponent(appId)}/deploy`,
      zshipBytes,
      { "Content-Type": "application/x-zship" },
    );
  }

  async updatePlan(appId: string, planId: string): Promise<{ updated: boolean }> {
    return this.fetchJson<{ updated: boolean }>(
      "PUT",
      `/api/apps/${encodeURIComponent(appId)}/plan`,
      { plan_id: planId },
    );
  }

  async getAppLogs(appId: string): Promise<string[]> {
    return this.fetchJson<string[]>("GET", `/api/apps/${encodeURIComponent(appId)}/logs`);
  }

  async listVars(appId: string): Promise<{ vars: EnvVar[] }> {
    return this.fetchJson<{ vars: EnvVar[] }>("GET", `/api/apps/${encodeURIComponent(appId)}/vars`);
  }

  async setVar(appId: string, key: string, value: string): Promise<void> {
    await this.fetchJson<void>("POST", `/api/apps/${encodeURIComponent(appId)}/vars`, {
      key,
      value,
    });
  }

  async deleteVar(appId: string, key: string): Promise<void> {
    await this.fetchJson<void>(
      "DELETE",
      `/api/apps/${encodeURIComponent(appId)}/vars/${encodeURIComponent(key)}`,
    );
  }

  async listSecrets(appId: string): Promise<{ secrets: string[] }> {
    return this.fetchJson<{ secrets: string[] }>(
      "GET",
      `/api/apps/${encodeURIComponent(appId)}/secrets`,
    );
  }

  async setSecret(appId: string, key: string, value: string): Promise<void> {
    await this.fetchJson<void>("POST", `/api/apps/${encodeURIComponent(appId)}/secrets`, {
      key,
      value,
    });
  }

  async deleteSecret(appId: string, key: string): Promise<void> {
    await this.fetchJson<void>(
      "DELETE",
      `/api/apps/${encodeURIComponent(appId)}/secrets/${encodeURIComponent(key)}`,
    );
  }

  private async fetchJson<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    return this.authorizedRequest<T>(
      method,
      path,
      body !== undefined ? JSON.stringify(body) : undefined,
      body !== undefined ? { "Content-Type": "application/json" } : {},
    );
  }

  private async fetchRaw<T>(
    method: string,
    path: string,
    body: BodyInit | Uint8Array,
    headers: Record<string, string>,
  ): Promise<T> {
    return this.authorizedRequest<T>(method, path, body, headers);
  }

  private async authorizedRequest<T>(
    method: string,
    path: string,
    body: BodyInit | Uint8Array | undefined,
    headers: Record<string, string>,
  ): Promise<T> {
    const tokens = await loadTokens(this.userId);
    if (!tokens) throw new OauthExpiredError("not logged in");

    const send = async (accessToken: string) => fetch(`${this.baseUrl}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${accessToken}`,
        ...headers,
      },
      body: body as BodyInit | undefined,
    });

    let resp = await send(tokens.access_token);
    if (resp.status === 401) {
      let fresh;
      try {
        fresh = await refreshAccessToken(tokens.refresh_token);
        await saveTokens(this.userId, {
          access_token: fresh.access_token,
          refresh_token: fresh.refresh_token,
          expires_at: Math.floor(Date.now() / 1000) + fresh.expires_in,
          scope: fresh.scope,
        });
      } catch (cause) {
        throw new OauthExpiredError("token refresh failed", { cause });
      }

      resp = await send(fresh.access_token);
      if (resp.status === 401) {
        throw new OauthExpiredError("token refused after refresh");
      }
    }

    if (!resp.ok) throw new ControlApiError(resp.status, await responseText(resp));
    if (resp.status === 204) return undefined as T;

    const contentType = resp.headers.get("content-type") ?? "";
    if (!contentType.includes("json")) return (await resp.text()) as T;
    return (await resp.json()) as T;
  }
}

export function getControlClient(req: Request | undefined = getRequest()): ControlClient {
  if (!req) throw new OauthExpiredError("not logged in");
  const userId = userIdFromRequest(req);
  if (!userId) throw new OauthExpiredError("not logged in");
  return new ControlClient({ userId });
}

async function responseText(resp: Response): Promise<string> {
  try {
    const text = await resp.text();
    return text || resp.statusText;
  } catch {
    return resp.statusText;
  }
}

function controlBaseUrl(): string {
  return readEnv(
    "ZEROSHIP_CONTROL_URL",
    readEnv("CONTROL_URL", "http://localhost:9090"),
  );
}

function stripTrailingSlash(value: string): string {
  return value.endsWith("/") ? value.slice(0, -1) : value;
}

function readEnv(key: string, fallback: string): string {
  const proc = (globalThis as {
    process?: { env?: Record<string, string | undefined> };
  }).process;
  const fromProcess = proc?.env?.[key];
  if (typeof fromProcess === "string" && fromProcess.length > 0) return fromProcess;

  const fromRuntime = (globalThis as { env?: Record<string, string | undefined> }).env?.[key];
  if (typeof fromRuntime === "string" && fromRuntime.length > 0) return fromRuntime;

  return fallback;
}
