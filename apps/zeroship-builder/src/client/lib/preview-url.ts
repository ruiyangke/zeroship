// Pure client-side URL builder for the deployed app preview iframe.
//
// In dev, vite proxies `/apps/<name>/` to the local zeroship-gate; in
// prod, the deployed app sits behind the production gateway with the
// same path-style routing. The function never talks to a server — it
// must NOT live in a `"use server"` module.

export function appPreviewUrl(appName: string, path: string = "/"): string {
  const base = `/apps/${encodeURIComponent(appName)}`;
  return path === "/"
    ? `${base}/`
    : `${base}${path.startsWith("/") ? "" : "/"}${path}`;
}
