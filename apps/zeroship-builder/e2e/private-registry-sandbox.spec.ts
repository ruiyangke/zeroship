import { test, expect } from "@playwright/test";

import {
  DEV_SANDBOX_USER_ID,
  ZeroshipSandboxBackend,
  getOrCreateSandboxFor,
  sdkRegistryNpmrcLine,
} from "../src/server/internal/sandbox-backend";

const STEP_TIMEOUT = 900_000;

function section(name: string, body: string): string {
  return `\n--- ${name} ---\n${body.trimEnd()}\n`;
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

async function readText(backend: ZeroshipSandboxBackend, path: string): Promise<string> {
  const [download] = await backend.downloadFiles([path]);
  if (!download || download.error || !download.content) {
    throw new Error(`failed to read ${path}: ${download?.error ?? "empty"}`);
  }
  return new TextDecoder().decode(download.content);
}

test.describe("private SDK registry sandbox consumption", () => {
  test("installs @zeroship/ui from Verdaccio and builds through the real docker sandbox", async ({}, testInfo) => {
    testInfo.setTimeout(STEP_TIMEOUT);

    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const sdkRegistry = process.env.ZEROSHIP_SDK_REGISTRY ?? "";
    const normalizedSdkRegistry = sdkRegistry.trim().replace(/\/+$/, "");
    test.skip(!process.env.SANDBOX_URL, "SANDBOX_URL unset; run scripts/e2e-private-registry-sandbox.sh");
    test.skip(!process.env.SANDBOX_TOKEN, "SANDBOX_TOKEN unset; run scripts/e2e-private-registry-sandbox.sh");
    test.skip(!sdkRegistry, "ZEROSHIP_SDK_REGISTRY unset; sandbox .npmrc needs a registry URL");
    test.skip(!(await probe(`${sandboxUrl}/readyz`)), `sandbox controller unreachable at ${sandboxUrl}`);

    const runId = crypto.randomUUID();
    const sandbox = await getOrCreateSandboxFor(`private-registry-${runId}`, {
      userId: DEV_SANDBOX_USER_ID,
      projectSourceId: runId,
    });
    const backend = new ZeroshipSandboxBackend({
      id: sandbox.id,
      userId: sandbox.userId,
    });

    const cleanup = await backend.execute(
      "find . -mindepth 1 -maxdepth 1 ! -name .npmrc -exec rm -rf {} +",
      { timeoutMs: 120_000 },
    );
    expect(cleanup.exitCode).toBe(0);

    const npmrc = await readText(backend, ".npmrc");
    const expectedNpmrcLine = sdkRegistryNpmrcLine(sdkRegistry);
    expect(expectedNpmrcLine).not.toBeNull();
    expect(npmrc).toContain(expectedNpmrcLine!);
    expect(npmrc).not.toMatch(/^registry=/m);
    console.log(section("sandbox .npmrc", npmrc));

    await backend.write(
      "package.json",
      `${JSON.stringify(
        {
          name: "zeroship-private-registry-e2e",
          version: "0.0.0",
          private: true,
          type: "module",
          scripts: {
            build: "vite build",
          },
          dependencies: {
            "@vitejs/plugin-react": "^6.0.1",
            "@zeroship/ui": "0.1.0",
            vite: "^8.0.10",
            react: "^19.2.5",
            "react-dom": "^19.2.5",
          },
          devDependencies: {},
        },
        null,
        2,
      )}\n`,
    );
    await backend.write(
      "index.html",
      [
        '<!doctype html>',
        '<html lang="en">',
        "  <head>",
        '    <meta charset="UTF-8" />',
        '    <meta name="viewport" content="width=device-width, initial-scale=1.0" />',
        "    <title>ZeroShip Registry E2E</title>",
        "  </head>",
        "  <body>",
        '    <div id="root"></div>',
        '    <script type="module" src="/src/main.tsx"></script>',
        "  </body>",
        "</html>",
        "",
      ].join("\n"),
    );
    await backend.write(
      "vite.config.ts",
      [
        'import { defineConfig } from "vite";',
        'import react from "@vitejs/plugin-react";',
        "",
        "export default defineConfig({",
        "  plugins: [react()],",
        "});",
        "",
      ].join("\n"),
    );
    await backend.write(
      "src/App.tsx",
      [
        'import { Button, Card, ThemeProvider } from "@zeroship/ui";',
        'import "@zeroship/ui/styles.css";',
        "",
        "export function App() {",
        "  return (",
        '    <ThemeProvider defaultTheme="atelier" storageKey="zs-private-registry-e2e-theme">',
        '      <main aria-label="private registry e2e">',
        "        <Card>",
        "          <h1>Verdaccio resolved</h1>",
        '          <p data-testid="registry-proof">Themed component rendered from @zeroship/ui.</p>',
        '          <Button variant="primary">Registry button</Button>',
        "        </Card>",
        "      </main>",
        "    </ThemeProvider>",
        "  );",
        "}",
        "",
      ].join("\n"),
    );
    await backend.write(
      "src/main.tsx",
      [
        'import { StrictMode } from "react";',
        'import { createRoot } from "react-dom/client";',
        'import { App } from "./App";',
        "",
        'createRoot(document.getElementById("root")!).render(',
        "  <StrictMode>",
        "    <App />",
        "  </StrictMode>,",
        ");",
        "",
      ].join("\n"),
    );

    const install = await backend.execute("pnpm install --frozen-lockfile || pnpm install", {
      timeoutMs: STEP_TIMEOUT,
    });
    console.log(section("pnpm install", install.output));
    expect(install.exitCode).toBe(0);

    const build = await backend.execute("pnpm build", { timeoutMs: STEP_TIMEOUT });
    console.log(section("pnpm build", build.output));
    expect(build.exitCode).toBe(0);

    const verify = await backend.execute(
      [
        "set -eu",
        'printf "pnpm-scope-registry="',
        "pnpm config get @zeroship:registry",
        'printf "verdaccio-tarball="',
        "pnpm view @zeroship/ui@0.1.0 dist.tarball",
        'printf "installed-package="',
        'node -e \'const p=require("./node_modules/@zeroship/ui/package.json"); process.stdout.write(p.name+"@"+p.version+"\\n")\'',
        'printf "ui-entry="',
        "node --input-type=module -e 'console.log(import.meta.resolve(\"@zeroship/ui\"))'",
        'printf "modules-yaml-registries\\n"',
        "sed -n '/^registries:/,/^[^ ]/p' node_modules/.modules.yaml || true",
        'printf "build-token-hits\\n"',
        "grep -Roh \"zs-theme-root\\|zs-button\" dist | sort -u",
        'printf "build-token-files\\n"',
        "grep -Rl \"zs-theme-root\\|zs-button\" dist | head -20",
      ].join("\n"),
      { timeoutMs: 120_000 },
    );
    console.log(section("registry/build verification", verify.output));
    expect(verify.exitCode).toBe(0);
    expect(verify.output).toContain(`pnpm-scope-registry=${normalizedSdkRegistry}`);
    expect(verify.output).toMatch(
      new RegExp(
        `verdaccio-tarball=${escapeRegExp(normalizedSdkRegistry)}/(?:@zeroship/ui|@zeroship%2fui)/-/ui-0\\.1\\.0\\.tgz`,
        "i",
      ),
    );
    expect(verify.output).toContain("installed-package=@zeroship/ui@0.1.0");
    expect(verify.output).toContain("/node_modules/.pnpm/@zeroship+ui@0.1.0");
    expect(verify.output).toContain(normalizedSdkRegistry);
    expect(verify.output).toMatch(/zs-theme-root|zs-button/);
  });
});
