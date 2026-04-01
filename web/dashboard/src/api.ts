// ── API Client ──────────────────────────────────────────────

export interface AppRecord {
  id: string;
  plan_id: string;
  version: number;
  api_key: string;
  created_at: string;
  updated_at: string;
}

export interface Stats {
  active_isolates: number;
  max_isolates: number;
  apps: { app_id: string; request_count: number; idle_secs: number }[];
}

export interface HealthStatus {
  status: string;
}

export interface UsageCounters {
  counters: Record<string, number>;
}

export interface AppUsage {
  app_id: string;
  counters: Record<string, number>;
}

function getKey(): string {
  return localStorage.getItem("appbase_key") || "";
}

async function request<T>(path: string, opts: RequestInit = {}): Promise<T> {
  const key = getKey();
  const res = await fetch(path, {
    ...opts,
    headers: {
      "Authorization": `Bearer ${key}`,
      ...opts.headers,
    },
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`${res.status} ${res.statusText}: ${text}`);
  }
  return res.json();
}

// ── Apps ──────────────────────────────────────────────────

export function listApps(): Promise<AppRecord[]> {
  return request("/api/apps");
}

export function getApp(id: string): Promise<AppRecord> {
  return request(`/api/apps/${id}`);
}

export function createApp(id: string, plan_id: string): Promise<AppRecord> {
  return request("/api/apps", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, plan_id }),
  });
}

export function deleteApp(id: string): Promise<{ deleted: boolean }> {
  return request(`/api/apps/${id}`, { method: "DELETE" });
}

export function deployApp(id: string, code: string): Promise<{ version: number }> {
  return request(`/api/apps/${id}/deploy`, {
    method: "POST",
    headers: { "Content-Type": "text/plain" },
    body: code,
  });
}

export function updatePlan(id: string, plan_id: string): Promise<{ updated: boolean }> {
  return request(`/api/apps/${id}/plan`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ plan_id }),
  });
}

// ── System ───────────────────────────────────────────────

export function getStats(): Promise<Stats> {
  return request("/_stats");
}

export function getHealth(): Promise<HealthStatus> {
  return request("/_health");
}

export function getAppUsage(id: string): Promise<UsageCounters> {
  return request(`/_apps/${id}/usage`);
}

export function getAllUsage(): Promise<AppUsage[]> {
  return request("/_usage");
}
