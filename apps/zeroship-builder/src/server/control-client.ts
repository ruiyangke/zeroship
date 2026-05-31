"use server";
// Control-plane client for the console (app-builder) running as a regular
// zeroship app on the standard worker runtime.
//
// Privilege model (MVP — see
// docs/superpowers/specs/2026-05-30-console-as-regular-app-design.md
// §"The privilege mechanism — MVP (ENV-var control credential)"):
//
//   - Identity (which creator is acting) comes from the PLATFORM session.
//     The gateway authenticates the creator and forwards the verified
//     `ZeroShip-User` envelope; the worker exposes it to app code as
//     `currentUser()` (the `zeroship` module). This replaces the old
//     bespoke OAuth-RP cookie/session surface (getRequest/userinfo).
//
//   - Authority (the right to mutate the control plane) comes from a
//     CONTROL SERVICE CREDENTIAL read from a SERVER-ONLY app env var
//     (`ZEROSHIP_CONTROL_SERVICE_TOKEN`). It is passed as the bearer to the
//     control plane via the `@zeroship/control` SDK. It is never the
//     creator's token, is never sent to the browser, and is never
//     returned in any response.
//
// Why a service PAT is the credential: the control plane's `AuthzGuard`
// bearer path (`crates/control/src/authz_guard.rs` `guard_from_bearer`)
// already accepts exactly one of: a control PAT (verified by
// `pat_issuer.verify` against a `control.permission_tokens` row) or an
// OAuth bearer (introspected). A PAT is a static, long-lived bearer
// string — the simplest credential the EXISTING control auth honors with
// ZERO new control-side machinery (no mint endpoint, no refresh dance).
// The `@zeroship/control` SDK takes it verbatim through its `auth`
// provider. (The richer per-request, identity-bound, scope-capped
// power-token is the DEFERRED full-R4 mint — out of scope here.)
//
// Per-creator scoping (ATTRIBUTION-ONLY until full-R4): the console
// threads the authenticated creator's id (`currentUser().id`) into every
// client as the `ZeroShip-Acting-User` header. This is an AUDIT
// BREADCRUMB, NOT enforced isolation: the control plane does not read
// `ZeroShip-Acting-User` today (it scopes every request to the service
// PAT's owner via `AuthzGuard::guard_from_bearer`, so all creators share
// the PAT's ownership/scope). Real per-creator isolation is the DEFERRED
// full-R4 power-token mint (an identity-bound, scope-capped, short-lived
// control bearer). Until then, do not assume this header enforces a
// tenant boundary.
//
// R4 follow-up: either teach the control plane to consume
// `ZeroShip-Acting-User` (and gate the service PAT to delegate to it) or
// revive the full power-token mint. See
// docs/superpowers/specs/2026-05-30-console-as-regular-app-design.md
// §"Future: full-R4 power-token mint".

import {
  createControlClient,
  ControlError,
  type ControlClient as ControlSdkClient,
} from "@zeroship/control";
import { currentUser } from "zeroship";
import { UpstreamServiceError } from "./internal/upstream-error.js";

export interface ControlClientOptions {
  /** The authenticated creator acting through the console. */
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

/**
 * Thrown when the console has no authenticated creator on the request.
 * Identity now comes from the platform session (`currentUser()`), so an
 * unauthenticated request can do nothing against the control plane.
 */
export class NotAuthenticatedError extends Error {
  constructor(message = "not authenticated") {
    super(message);
    this.name = "NotAuthenticatedError";
  }
}

export class ControlApiError extends UpstreamServiceError {
  constructor(status: number, body: string, operation: string) {
    super({
      service: "control",
      operation,
      status,
      body,
      publicMessage: "control request failed",
    });
    this.name = "ControlApiError";
  }
}

export class ControlClient {
  private readonly userId: string;
  private readonly sdk: ControlSdkClient;

  constructor(opts: ControlClientOptions) {
    if (!opts.userId) throw new Error("userId is required");
    this.userId = opts.userId;
    const baseUrl = stripTrailingSlash(opts.baseUrl ?? controlBaseUrl());
    this.sdk = createControlClient({
      baseUrl,
      // Server-only control service credential. Resolved lazily so a
      // missing credential surfaces only when a control call is actually
      // made (and never at module/app-boot time in the browser bundle).
      auth: () => controlServiceToken(),
      // Carry the acting creator's id as an ATTRIBUTION breadcrumb (for
      // audit logs) — NOT enforced isolation. The control plane does not
      // read this header today; every request is authorized as the
      // service PAT's owner (`AuthzGuard::guard_from_bearer`). Enforced
      // per-creator scoping is the deferred full-R4 power-token mint.
      headers: () => ({ "ZeroShip-Acting-User": this.userId }),
    });
  }

  async listApps(): Promise<App[]> {
    return this.call(() => this.sdk.apps.list() as Promise<App[]>, "GET /api/apps");
  }

  async getApp(id: string): Promise<App> {
    return this.call(
      () => this.sdk.apps.get(id) as Promise<App>,
      `GET /api/apps/${id}`,
    );
  }

  async createApp(name: string, planId: string): Promise<App> {
    return this.call(
      () => this.sdk.apps.create({ name, plan_id: planId }) as Promise<App>,
      "POST /api/apps",
    );
  }

  async deleteApp(id: string): Promise<{ deleted: boolean }> {
    return this.call(
      () => this.sdk.apps.delete(id),
      `DELETE /api/apps/${id}`,
    );
  }

  async deploy(appId: string, zshipBytes: Uint8Array): Promise<DeployResult> {
    return this.call(
      // Wrap the bytes as a Blob — `BodyInit` typings vary over whether a
      // raw typed-array is accepted; a Blob is universally accepted.
      () =>
        this.sdk.apps.deploy(appId, new Blob([zshipBytes as BlobPart]), {
          contentType: "application/x-zship",
        }) as Promise<DeployResult>,
      `POST /api/apps/${appId}/deploy`,
    );
  }

  async updatePlan(appId: string, planId: string): Promise<{ updated: boolean }> {
    return this.call(
      () => this.sdk.apps.setPlan(appId, { plan_id: planId }),
      `PUT /api/apps/${appId}/plan`,
    );
  }

  async getAppLogs(appId: string): Promise<string[]> {
    return this.call(
      () => this.sdk.apps.logs(appId),
      `GET /api/apps/${appId}/logs`,
    );
  }

  async listVars(appId: string): Promise<{ vars: EnvVar[] }> {
    return this.call(
      () => this.sdk.env.listVars(appId),
      `GET /api/apps/${appId}/vars`,
    );
  }

  async setVar(appId: string, key: string, value: string): Promise<void> {
    await this.call(
      () => this.sdk.env.setVar(appId, { key, value }),
      `POST /api/apps/${appId}/vars`,
    );
  }

  async deleteVar(appId: string, key: string): Promise<void> {
    await this.call(
      () => this.sdk.env.deleteVar(appId, key),
      `DELETE /api/apps/${appId}/vars/${key}`,
    );
  }

  async listSecrets(appId: string): Promise<{ secrets: string[] }> {
    return this.call(
      () => this.sdk.env.listSecrets(appId),
      `GET /api/apps/${appId}/secrets`,
    );
  }

  async setSecret(appId: string, key: string, value: string): Promise<void> {
    await this.call(
      () => this.sdk.env.setSecret(appId, { key, value }),
      `POST /api/apps/${appId}/secrets`,
    );
  }

  async deleteSecret(appId: string, key: string): Promise<void> {
    await this.call(
      () => this.sdk.env.deleteSecret(appId, key),
      `DELETE /api/apps/${appId}/secrets/${key}`,
    );
  }

  /**
   * Run an SDK call, mapping `@zeroship/control` transport failures to a
   * `ControlApiError` that never leaks upstream bodies (the upstream body
   * is logged with a request id by `UpstreamServiceError`, not surfaced).
   */
  private async call<T>(fn: () => Promise<T>, operation: string): Promise<T> {
    try {
      return await fn();
    } catch (err) {
      if (err instanceof ControlError) {
        throw new ControlApiError(err.status, controlErrorBody(err), operation);
      }
      throw err;
    }
  }
}

export function getControlClient(): ControlClient {
  const userId = currentCreatorId();
  if (!userId) throw new NotAuthenticatedError();
  return new ControlClient({ userId });
}

/**
 * The acting creator's id, from the platform session the gateway forwards
 * as `ZeroShip-User` (exposed to app code as `currentUser()`). Returns
 * `null` when there is no authenticated user on the current request.
 */
function currentCreatorId(): string | null {
  let user: { id?: unknown } | null;
  try {
    user = currentUser() as { id?: unknown } | null;
  } catch {
    // `currentUser()` throws outside a request handler.
    return null;
  }
  return typeof user?.id === "string" && user.id.length > 0 ? user.id : null;
}

function controlErrorBody(err: ControlError): string {
  if (typeof err.body === "string") return err.body;
  if (err.body != null) {
    try {
      return JSON.stringify(err.body);
    } catch {
      /* fall through */
    }
  }
  return err.statusText || `HTTP ${err.status}`;
}

/**
 * The control service credential (a control PAT). SERVER-ONLY: read from
 * the app's server-side env, never exposed to the browser and never
 * returned in a response. Throws if unset so a misconfigured deploy fails
 * loudly rather than silently making unauthenticated control calls.
 */
function controlServiceToken(): string {
  const token = readEnv("ZEROSHIP_CONTROL_SERVICE_TOKEN", "");
  if (!token) {
    throw new Error(
      "ZEROSHIP_CONTROL_SERVICE_TOKEN is not set. The console authenticates to the " +
        "control plane with a server-only control service credential (a control " +
        "PAT); set ZEROSHIP_CONTROL_SERVICE_TOKEN as a server-side app secret.",
    );
  }
  return token;
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
