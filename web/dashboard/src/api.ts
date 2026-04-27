// ── API Client ──────────────────────────────────────────────

export interface AppRecord {
  id: string;             // UUID — primary key
  name: string;           // creator-chosen slug (used for routing)
  plan_id: string;
  deploy_hash: string | null;
  api_key: string;
  created_at: string;
  updated_at: string;
  server_js?: string;     // included when fetching single app
}

export interface Stats {
  active_isolates: number;
  max_isolates: number;
  apps: { app_id: string; request_count: number; idle_secs: number }[];
}

export interface HealthStatus {
  status: string;
}

// /_apps/:id/usage returns flat { requests: N, cpu_us: N, ... }
export type UsageCounters = Record<string, number>;

// /_usage returns { app_id: { resource: value, ... } }
export type AllUsage = Record<string, Record<string, number>>;

const API_BASE = '';

function getKey(): string {
  return localStorage.getItem("zeroship_key") || "";
}

async function apiFetch<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    ...options,
    headers: {
      'Authorization': `Bearer ${getKey()}`,
      'Content-Type': 'application/json',
      ...options?.headers,
    },
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `HTTP ${res.status}`);
  }
  return res.json();
}

// ── Apps ──────────────────────────────────────────────────

export function listApps(): Promise<AppRecord[]> {
  return apiFetch("/api/apps");
}

export function getApp(id: string): Promise<AppRecord> {
  return apiFetch(`/api/apps/${id}`);
}

export function createApp(name: string, plan_id: string): Promise<AppRecord> {
  return apiFetch("/api/apps", {
    method: "POST",
    body: JSON.stringify({ name, plan_id }),
  });
}

/** URL where a deployed app is reachable (path-based for local dev). */
export function appPreviewUrl(appName: string, path: string = "/"): string {
  const base = `/apps/${encodeURIComponent(appName)}`;
  return path === "/" ? `${base}/` : `${base}${path.startsWith("/") ? "" : "/"}${path}`;
}

/** Upload a static asset (HTML/CSS/JS file). */
export function uploadAsset(appId: string, path: string, content: string, contentType: string): Promise<{ uploaded: string; size: number }> {
  return apiFetch(`/api/apps/${appId}/assets/${path}`, {
    method: "PUT",
    headers: { "Content-Type": contentType },
    body: content,
  });
}

export function deleteApp(id: string): Promise<{ deleted: boolean }> {
  return apiFetch(`/api/apps/${id}`, { method: "DELETE" });
}

export function deployApp(id: string, code: string): Promise<{ deploy_hash: string }> {
  return apiFetch(`/api/apps/${id}/deploy`, {
    method: "POST",
    headers: { "Content-Type": "application/javascript" },
    body: code,
  });
}

export function updatePlan(id: string, plan_id: string): Promise<{ updated: boolean }> {
  return apiFetch(`/api/apps/${id}/plan`, {
    method: "PUT",
    body: JSON.stringify({ plan_id }),
  });
}

// ── System ───────────────────────────────────────────────

export function getStats(): Promise<Stats> {
  return apiFetch("/_stats");
}

export function getHealth(): Promise<HealthStatus> {
  return apiFetch("/_health");
}

export function getAppUsage(id: string): Promise<UsageCounters> {
  return apiFetch(`/_apps/${id}/usage`);
}

export function getAllUsage(): Promise<AllUsage> {
  return apiFetch("/_usage");
}

// ── Logs ──────────────────────────────────────────────────

export function getAppLogs(id: string): Promise<string[]> {
  return apiFetch(`/api/apps/${id}/logs`);
}

// ── Templates ─────────────────────────────────────────────

export interface AppTemplate {
  id: string;
  name: string;
  description: string;
  code: string;
}

export function getTemplates(): Promise<AppTemplate[]> {
  return apiFetch("/api/templates");
}

// ── RPC ───────────────────────────────────────────────────

export async function callRpc(appId: string, method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch('/rpc', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'X-App-Id': appId,
    },
    body: JSON.stringify({ jsonrpc: '2.0', method, params, id: 1 }),
  });
  return res.json();
}
