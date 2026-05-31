import { describe, expect, it } from "vitest";
import { shouldProxySsrDevPath } from "../vite.config";

describe("SSR dev proxy routing", () => {
  it("proxies document routes to the zeroship runtime", () => {
    expect(shouldProxySsrDevPath("/")).toBe(true);
    expect(shouldProxySsrDevPath("/post/first")).toBe(true);
    expect(shouldProxySsrDevPath("/docs/nested?draft=1")).toBe(true);
  });

  it("leaves Vite-owned and API paths on the Vite dev server", () => {
    expect(shouldProxySsrDevPath("/@vite/client")).toBe(false);
    expect(shouldProxySsrDevPath("/@react-refresh")).toBe(false);
    expect(shouldProxySsrDevPath("/@id/react")).toBe(false);
    expect(shouldProxySsrDevPath("/@fs/workspace/app/file.ts")).toBe(false);
    expect(shouldProxySsrDevPath("/__vite_ping")).toBe(false);
    expect(shouldProxySsrDevPath("/src/entry-client.tsx?t=123")).toBe(false);
    expect(shouldProxySsrDevPath("/node_modules/.vite/deps/react.js")).toBe(false);
    expect(shouldProxySsrDevPath("/assets/index.js")).toBe(false);
    expect(shouldProxySsrDevPath("/__zeroship/v1/listPosts")).toBe(false);
    expect(shouldProxySsrDevPath("/__zeroship_runtime")).toBe(false);
    expect(shouldProxySsrDevPath("/favicon.ico")).toBe(false);
    expect(shouldProxySsrDevPath("/robots.txt?cache=0")).toBe(false);
  });
});
