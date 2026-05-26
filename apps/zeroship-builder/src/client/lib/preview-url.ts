// Pure client-side URL builders for preview surfaces.
//
// In dev, vite proxies `/apps/<name>/` to the local zeroship-gate; in
// prod, the deployed app sits behind the production gateway with the
// same path-style routing. The function never talks to a server — it
// must NOT live in a `"use server"` module.

// Post-deploy preview: public app URL behind the gateway.
export function appPreviewUrl(appName: string, path: string = "/"): string {
  const base = `/apps/${encodeURIComponent(appName)}`;
  return path === "/"
    ? `${base}/`
    : `${base}${path.startsWith("/") ? "" : "/"}${path}`;
}

// Live sandbox preview: builder-origin server-side proxy. The browser
// hits this route; the builder runtime forwards to the sandbox
// controller with SANDBOX_TOKEN server-side.
export function liveSandboxPreviewUrl(
  appId: string,
  port: number,
  path: string = "/",
): string {
  const base = `/api/preview/${encodeURIComponent(appId)}/${port}`;
  return path === "/"
    ? `${base}/`
    : `${base}${path.startsWith("/") ? "" : "/"}${path}`;
}
