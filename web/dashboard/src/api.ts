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

// /_apps/:id/usage returns flat { requests: N, cpu_us: N, ... }
export type UsageCounters = Record<string, number>;

// /_usage returns { app_id: { resource: value, ... } }
export type AllUsage = Record<string, Record<string, number>>;

const API_BASE = '';

function getKey(): string {
  return localStorage.getItem("appbase_key") || "";
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

export function createApp(id: string, plan_id: string): Promise<AppRecord> {
  return apiFetch("/api/apps", {
    method: "POST",
    body: JSON.stringify({ id, plan_id }),
  });
}

export function deleteApp(id: string): Promise<{ deleted: boolean }> {
  return apiFetch(`/api/apps/${id}`, { method: "DELETE" });
}

export function deployApp(id: string, code: string): Promise<{ version: number }> {
  return apiFetch(`/api/apps/${id}/deploy`, {
    method: "POST",
    headers: { "Content-Type": "text/plain" },
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
